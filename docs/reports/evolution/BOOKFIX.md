# BOOKFIX — locked-top 盘口语义修复 + 策略复活（#406 / OPT-v2 §5 根因）

状态：定稿（全部数字已实测回填，可复算——命令与工件路径见文末）。

## 1. 根因（OPT-v2 §5.1 的修复记录）

`core/blitzkrieg_core/src/marketdata.rs` `update_top`（price_change / best_bid_ask 的
top-of-book 路径）原实现：

```
asks.retain(|p, _| *p > b);   // 第一段：清掉 <= b 的卖价
bids.retain(|p, _| *p < a);   // 第二段：清掉 >= a 的买价
```

locked top（bb == ba，做市商双边同价的**合法**市场状态）下两段边界都吃到共享价位：
第一段删掉 ==b 的卖价、第二段删掉 ==a 的买价 → 盘口塌成单边 →
`from_sorted_levels` 的 F6 规则（单边书不给 mid）→ `mid_price = 0` →
一切依赖 `mid > 0` 的策略状态机（spread_arb 的 TrendTracker、lua_momentum 的
classify）永不推进 = 结构性 0 信号。交易组（pair/single_leg/oracle）用 best_ask
定价，locked 时幸存——这就是「能交易的不看 mid、看 mid 的不能交易」的分工。

**语料证据**：全量 v2 语料（356,749 事件）= 293,286 个 top 事件，
**100.0000% locked（bb==ba），0 crossed，0 normal**；book 快照事件 0 个。
该语料上盘口状态 100% 走 `update_top`。

## 2. 修复语义（行为由 marketdata.rs 测试锁定）

- **inclusive bounds**：只删**严格劣于**对侧 best 的档位（`>= b` / `<= a`）。
  locked top 后双边都保留共享价位，mid = 该价位，best_bid/best_ask 都有值。
- **crossed 输入（bb > ba，数据毛刺）**：inclusive bounds 自身会扫掉不可能的
  quote——fresh book 上 crossed top 退化为 ask 单边（买方保守侧幸存）；
  既有档位残存交叉时由收尾 clamp 把 bid 压到 best_ask（book 锁死在该价）。
  不变量：snapshot 永不呈现 crossed spread。测试：
  `top_update_crossed_input_never_presents_a_crossed_book` 等 8 个新测试。
- 深度语义不变：top 更新不携带 size，仍一致的老档位保留（与修复前一致，
  只有「严格劣于」的档位被清）。

## 3. 任务 0：为什么 exit-economics 钉死窗口能成交（窗口差异诊断）

**结论：exit-economics 基线不需要重录。**

frozen corpus（scripts/lib/frozen-corpus.mjs materialize 的 4 窗口）与全量 v2
语料的数据面构成完全不同：

| 语料 | book 快照 | top 事件 | 盘口路径 |
|---|---|---|---|
| frozen corpus（4×1h，2026-09-19/20） | 27,064 | **0** | `apply_snapshot`（不经过 update_top） |
| 全量 v2（2026-10-01 起 90h） | 0 | 293,286（100% locked） | `update_top`（bug 路径） |

spread_arb 在钉死窗口能成交，是因为窗口重放喂的是 book 全量快照——
`apply_snapshot` 无此 bug，mid 一直健康；全量 v2 只有 top 事件，
`update_top` 把每个 locked top 都变成单边书 → mid=0 → 0 信号。
「窗口能成交、全量不能」不是策略参数差异，是**数据面走了不同代码路径**。

门禁实测（BK_CORE_BIN=修复版 core）：4 窗口与 2026-10-02 基线（ea347172 录制）
**逐位一致**（13/30.77%/+5.2070、12/16.67%/−0.2262、6/16.67%/−0.3556、
13/0%/−11.0845）——直接证明修复未触碰 book 路径语义。基线原样保留，
tolerance 未动，--self-test 8 fixtures 绿，--teeth 绿。

## 4. #406：0.00 价格 intent 的除零 panic（fail-closed guard）

**根因定位**（debug build backtrace 实证）：panic 点不在 arbitration Gate 1
（`OrderIntent::validate` 本就有 Prediction price∈(0,1] 校验），而在更早的
**execution-policy Place 臂 `budget_usd / req.price`**（service.rs，#363 预算
上限逻辑）——policy verdict 在 Gate 1 之前执行，price=0 intent 直达除法即崩。
rust_decimal 的 `Division by zero` panic 信息掩盖了真实位置；OPT-v2 §5.2 归因的
「physics 路径」实为同一 panic 的误读（`apply_physics` 只有乘法）。

**修复**：placement loop 头部（一切 policy/sizing/physics 之前）对
`side == Buy && price <= 0` 的 entry fail-closed 拒绝：
- `tracing::error!` 日志（策略名 + token + 价格）；
- 会话计数器 `illegal_price_rejected`（`engine.stats.illegalPriceRejected`，
  ui_kit EngineStatsView 同步 carry）；
- per-strategy 归因桶 `illegal.price`（E9-c 口径）；
- closes 豁免（出场定价不走策略报价）。
测试 `zero_price_entry_is_refused_fail_closed_not_a_panic`：单边书 → mid=0
信号 → price=0.00 intent；修复前此测试 panic，修复后 0 订单 + 计数器 = 1 +
进程存活。

## 5. 复活普查（train 语料中段 5×20K 切片，单开，命令见 §7）

**结论：盘口结构修复让依赖 mid 的策略从结构性 0 成交恢复为正成交。**

| 策略 | base closed | fixed closed | fixed WR | fixed net (USD) |
|---|---|---|---|---|
| spread_arb | **0** | 782 | 10.1% | −100.93 |
| lua_momentum | **0** | 5,687 | 51.4% | −5,221.53 |
| pair_discount_arb（回归红线） | 4,921* | 4,921* | — | 逐位一致 |
| single_leg_pair（回归红线） | 3,675* | 3,675* | — | 逐位一致 |

*红线口径见 §5.1。spot veto 说明：flash_arb / market_maker 未进普查——前者在
shadow evolution 的 spot veto 名单上（E9 纪录），后者做市报价路径不依赖 mid
信号机；两者与 locked-top 修复无因果面。

「有信号但净亏」是策略质量层（threshold/费用占名义比），不是结构层：
修复前这些策略连状态机都无法推进，任何调参都无从谈起。

### 5.1 逐位回归红线（ask 侧语义不变证明）

全量 v2 语料 356,749 行等距切 8 块（probe/full-part{1..8}.jsonl，各 44,600 行），
base 二进制 vs 修复版二进制，单开回放，trades JSON 全等对比：

- pair_discount_arb：8/8 分块 **MATCH**（4,921 笔 vs 4,921 笔，逐位一致）
- single_leg_pair：8/8 分块 **MATCH**（3,675 笔 vs 3,675 笔，逐位一致）

交易组策略用 best_ask 定价、不读 mid，修复后成交流完全不变——
「复活」只发生在原本被 mid=0 锁死的策略上。

## 6. 组合表（train/val 纪律，bk-agentopt 切分）

4 策略组合（pair_discount_arb + single_leg_pair + spread_arb + lua_momentum），
修复版二进制，官方费率。train 8 块 / val 5 块（每块独立状态清理），全部 EXIT=0。

### 组合口径

| 语料 | closed | WR | grossProfit | grossLoss | PF | net (USD) |
|---|---|---|---|---|---|---|
| train（222,945 行） | 14,318 | 45.6% | 6,488.62 | 14,359.43 | **0.452** | −7,870.81 |
| val（133,804 行） | 8,340 | 43.9% | 3,833.94 | 8,718.11 | **0.440** | −4,884.16 |

### 按策略（train / val）

| 策略 | closed | WR | net (USD) |
|---|---|---|---|
| lua_momentum | 13,009 / 7,637 | 44.6% / 43.4% | −7,885.17 / −4,858.84 |
| pair_discount_arb | 353 / 162 | 64.0% / 56.2% | +16.20 / +1.84 |
| single_leg_pair | 779 / 405 | 63.8% / 60.5% | +26.06 / +7.87 |
| spread_arb | 177 / 136 | 13.0% / 9.6% | −27.91 / −35.03 |

### 「离 65% / PF3 还有多远」

- **复活增量**：组合 closed 减去 base 就活的 pair+SL（train 1,132 / val 567）→
  修复带来 train 13,186 + val 7,773 = **20,959 笔此前完全不存在的成交**
  （spread_arb 313 + lua_momentum 20,646）。
- **WR 65% 一线**：双 arb（pair_discount_arb、single_leg_pair）train 已在
  63.8–64.0%、val 56–60%——离 65% 一线之隔且 train/val 方向一致；这两个
  策略的成交流与 base 逐位一致（结构层无回归）。
- **PF 3.0 差距**：组合 PF 0.45 vs 目标 3.0，差距几乎全部来自
  lua_momentum（占组合成交量 91%、净亏 100%+）与 spread_arb 的 WR 13%
  （每笔成交经济学为负）。这是**策略质量/参数层**（threshold、费用占比、
  拒单经济学——lua_momentum 每 1 笔成交伴随 111 次拒单），不是盘口结构层：
  结构层（mid=0 → 状态机永不推进）已被本修复消灭，工程侧已无可再修的
  沉默失败。下一步在策略参数空间（--backtest-knob），不在内核。

## 7. 复算路径

- base 二进制（修复前，fb74c65f 干净构建）：`/Volumes/Hard Disk/bk-session/bin/core-bookfix-base`
- 修复版二进制：`/Volumes/Hard Disk/bk-session/bin/core-bookfix-fixed`
- 报告 JSON：`/Volumes/Hard Disk/bk-agentbookfix/reports/{base,fixed}-<strategy>.json`
  （复活普查 + 逐位回归），组合表 `combo4-{train,val}-c{0..7|0..4}.json`
- 分块语料：`/Volumes/Hard Disk/bk-agentbookfix/probe/`
  （`full-part{1..8}.jsonl` 全量切 8；`train-combo-c{0..7}.jsonl`、
  `val-combo-c{0..4}.jsonl` train/val 切块；`chunk-{a..e}.jsonl` train 中段 5×20K 普查切片）
- 回放日志：`/Volumes/Hard Disk/bk-agentbookfix/logs/`
- 回放命令模板：`/Volumes/Hard Disk/bk-session/README.md`（回放前必须
  `rm -f data/strategy-state.json data/positions.daily-loss.json`，cwd=该目录；
  每块独立回放，块间无状态共享——组合表口径 = 各块统计相加）
- 语料（只读）：`/Volumes/Hard Disk/BlitzkriegBot/data/onchain/3725d52f…-15m-act-4bf06519abf5.jsonl`
- train/val 切分：`/Volumes/Hard Disk/bk-agentopt/corpus-v2-{train,val}.jsonl`
  （train 1785949200000–1788479955000，val 1788480000000–1790787600000；
  train 222,945 行 / val 133,804 行）
- 后台收割对策（本报告全部数字的执行方式）：harness 后台 Bash ~34min 硬收割
  （exit 137），故全部回放以前台分块完成——单块 ≤28K 行约 1.5–3min。
