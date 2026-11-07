# coin_flip_exit_lab — #962 Exit-First 实证报告

**判决（验收 3 二选一）：期望 ≤ 0。出场机制在随机入场下不能独立贡献正期望；
对照组（固定呆板出场）反而显著好于全部 16 个动态栈网格点。**

- 动态出场栈（primary `h20_tp30_tr25_dt0`）：E[trade] = **−$0.0472**（净 −$255.83 / 5,422 笔）
- 固定呆板出场（control-rigid）：E[trade] = **−$0.0260**（净 −$101.39 / 3,902 笔）
- 16 点预声明网格：**全部为负**（−$252.64 ～ −$258.27），无一翻正

## 预声明坐标（数字存在之前钉死，脚本内常量）

| 项 | 值 |
|---|---|
| seed | 962（`--backtest-knob coin_flip_exit_lab:coin_seed=962` 显式下发） |
| 语料 | `data/onchain/` 15m-act 单文件（钱包 0x3725…08ad88，2026-08-05→10-31，9,780 轮次市场）；处理副本 sha256 `2787348f07c3…` 钉在脚本 `CORPUS_SHA256` |
| 语料闸 | `fix_resolutions.py --check` 8,034 结算行 bad=0；`relabel_official.py --apply-corpus` 全量重写后与 pull manifest events sha **逐字节相同**（apply 实为全量官方真值复核，非修正） |
| 写实档 | 每臂 `--fee-model official --slippage-ticks 1 --backtest-tick-ms 50 --backtest-fast` |
| sizing | 1 股（act 语料合成盘名义深度 1.0，FOK 杀更大单——语料坐标决定，非调参；两臂同值） |
| 轮次窗 | `round_min_seen_time_left=885`（转换器首个 top 行落轮内 ≤15s），9,780 轮次市场全进 |
| 网格 | hard{20,35} × tp{15,30} × trail{10,25} × death_tight{0,10} = 16 点，**全部报告** |
| primary | `h20_tp30_tr25_dt0`（= manifest 出厂默认减去语料节奏 stale 超时，跑前声明） |
| 对照 | 同包同 seed，出场栈全清零（hard 0 / tp 0(+tight) / trail 0(+tight) / death 100,0 / stale 0），只剩 `exit_time_left_sec=45`——与 `scripts/ab-exit-964.mjs CONTROL_KNOBS` 逐字一致 |
| ⚠️ stale 发现 | stdlib 30s quoting 超时在 ~60s 节奏的 act 语料上，仓位第 2 个 top 到达即判死：`shipped-default` 臂整列塌缩（5,428/5,429 死于 ThesisDeathExit，WR 0.0%，净 −$295.89）。网格全部 `thesis_death_stale_ms=0`（超时关；盘口消失仍判死那条腿每臂共享）；shipped-default 臂留档于归因 |

## 验收 1：确定性 ✅

primary 双跑（同语料同 seed，独立 staged 包）：**IDENTICAL**——
`trades_sha256 = b810cc4051729745afa05fbb5752eaa5cc8254dafe01560debeab7019aed6584` 两次一致，归一化 staged 路径后报告逐字节相同（2,568,258 bytes）。附 JS 哈希交叉核对：**全部 18 臂每笔已实现方向与 FNV-1a-64+splitmix64 预测零失配**（shipped 5,429/5,429、control 3,902/3,902、网格逐臂 ✓=closed/1 失配），单 flip 单仓契约 `duplicateSlotCond=0`。

## 验收 2：样本与分布 ✅（样本量 9,780 轮次市场，5,422 笔 primary）

| 臂 | closed | WR | 净 PnL | E[trade] | 最差单笔 | 平均持仓 |
|---|---|---|---|---|---|---|
| shipped-default（stdlib 30s 超时原样） | 5,429 | 0.0% | −$295.89 | −$0.0545 | −$0.13 | 61s |
| control-rigid（呆板出场） | 3,902 | 50.2% | −$101.39 | −$0.0260 | −$0.78 | 857s |
| primary h20_tp30_tr25_dt0 | 5,422 | 42.2% | −$255.83 | −$0.0472 | −$0.52 | 153s |
| 网格最优 h35_tp30_tr10_dt10 | 5,423 | 42.3% | −$252.64 | −$0.0466 | −$0.54 | 158s |
| 网格最差 h35_tp30_tr25_dt0 | 5,412 | 47.0% | −$258.27 | −$0.0477 | −$0.54 | 180s |

primary 出场原因直方图：ThesisDeathExit 1,991 / TakeProfit 2,270 / StrategySignal 1,118 / TrailingStop 43；费 $173.52（官方 taker 档）。control：TimeExit 2,719 / Settlement 1,180 / UnverifiableExpiry 3；费 $79.16。

## 验收 3：二选一判决与归因（禁止含糊）

**期望 ≤ 0，缺口归因：**

1. **入场不是背锅侠，出场也不是救主**——control 把动态栈全关反而 E 最优（−0.0260 vs 全网格 −0.0466~−0.0477）。原因结构清晰：随机入场下两向对半，期望由**手续费 × 换手**主导。动态栈每笔提前平仓=每笔重付 taker 费，把「少亏在崩腿」的收益全部交给费用吃掉（primary 费 $173.52 vs control $79.16，费差 $94.36 ≈ 净差 $154.44 的六成）。
2. **提前出场在此语料上买不到保护**——本语料入场价贴近 0.5 中位（合成盘 taker 吃 ask），崩腿从 0.5→0 的路径上 hard_stop 20%/35% 都在费用后的 net 判据下触发，止损费 > 持有到结算的期望损失改善；TakeProfit 2,270 笔锁住的盈利又被下一轮随机单重新下注。
3. **stale 超时与语料节奏冲突**（shipped-default 臂实测）——stdlib 30s quoting 超时假设逐秒盘口；act 语料 ~60s/top，第 2 个 top 即判死。这解释了为何 `--backtest-knob` 网格必须 stale=0；生产 archive 语料（秒级 book）无此问题。
4. **对比 lua_momentum 真信号**——0.4/0.5 时代的 maker-first 入场（负 fee）+ 动量方向过滤在本同语料族上曾录得正期望；本实验把入场 alpha 拆零后 E 直接落回「费率 × 换手」地板，两相印证：**edge 在入场侧的 maker 结构与方向选择，不在出场栈**。

**结论**：出场机制在随机入场下期望贡献为负（费用主导）；「完美动态出场」不能救随机入场。运营者哲学的 Exit-First 假设在 15m 二项市场、官方 taker 费率、1 tick 摩擦的写实档下**证伪**。

## 验收 4：可追溯性

| 产物 | 路径 |
|---|---|
| 18 臂原始回放报告 | `/tmp/bk-coin-flip-lab/<arm>.json`（36MB；summary.json 含全臂聚合+确定性记录） |
| 实验脚本（预声明坐标+网格+对照+双跑） | `scripts/coin-flip-lab-replay.mjs` |
| JS 哈希交叉核对实现 | `scripts/lib/coin-flip.mjs`（每笔已实现方向逐一复核） |
| 处理后语料（gitignored） | `data/corpus/coin-flip-962/onchain-15m-act-official.jsonl`（sha 钉在脚本） |
| 持仓时长 | 回放 intent audit 重推（±1 evaluate tick）；primary unmeasured=0，kernelDerived=0 |

每个数字 = summary.json 对应字段直出或四则组合；复跑：`node scripts/coin-flip-lab-replay.mjs --replay`（约 18 分钟，全臂+determinism）。

## 边界（如实声明）

- act 语料合成盘口（bid==ask、~60s 节奏、名义深度 1.0）：maker 出场成交偏乐观、taker 入场口径为本坐标唯一可行入场；**结论换生产 archive 复核后方可进生产**。
- 持仓时长=重推近似（声明口径），非逐笔时间戳（引擎 TradeLine 无时间戳，#964 报告同款限制）。
- 单 seed 962；跨 seed 方差未测（预声明网格 16 点已全负，方差救不回 −$253 的地板）。

## 禁止触碰核对

- 内核/风控/UI 零修改 ✅（回测引擎只读；对照清零全走 `--backtest-knob` hot-param）
- 参数只按预声明网格跑、全结果报告，无 cherry-pick ✅
- 无未来数据（语料闸 + sha 钉死 + 两道 python 门禁）✅
- LAB ONLY：不进 `user_layer/configs/*`，manifest 自声明禁止实盘默认 ✅
