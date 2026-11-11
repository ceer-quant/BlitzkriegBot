# lua_momentum 止损机制解剖报告 (#1075)

**基线：** `ceer/main` (0d6169cc)
**分支：** `fix/1075-stop-loss-audit`
**数据源：** `data/trades/trades.jsonl`（策略 `lua_momentum`，场 `polysimulator`，1060 笔成交；另 38 笔非 polysimulator 场未计入主表）、`data/orders/orders.jsonl`
**报告方式：** 只读数据分析，**未改策略代码**，未改 `stop_loss_pct` 值

---

## 1. 止损逻辑梗概（供核对）

**`user_layer/strategies_lua/lua_momentum/strategy.lua` (`consider_exit`)**

- 止损参数：`stop_loss_pct_down = 50`、`stop_loss_pct_up = 30`（入场价绝对百分点，0 禁用）。
- 止损基准：**入场价绝对百分点**，与复合止盈共用同一套 `entry` 基准：`proceeds <= entry * (100 - sl) / 100`，即净卖出价（bid − 手续费）跌破入场价的 (100−sl)% 即触发。判定用 **NET proceeds**（扣手续费后），比名义更保守。
- 评估顺序（对每个持仓代币，每次 `bk_evaluate`）：
  1. 若无**fresh 且带 best_bid 的盘口** → 产出意图 `stop_blocked_no_liquidity`，**不下单，直接返回**（后面的止盈/追踪/时间止全部不再评估）。持仓保留。
  2. 若止损幅度已 breach → 产出意图 `stop_loss`，返回。内核以 **best bid 为卖出价**（taker）下单成交。
  3. 否则依次评估 `take_profit` / `take_profit_price` / `trailing_stop` / `time_exit`。
- 止损属“最后手段”容错位，不是主出场：主出场是 `take_profit` / `trailing_stop`。

**`core/blitzkrieg_core/src/service/risk_admission/exits.rs` (`exit_reason_from_strategy_tag`)**

- `"stop_loss" | "stop_blocked_no_liquidity" => ExitReason::StopLoss`（#1072 已将这两个标签从 `StrategySignal` 兜底中分离，映射为真实的止损原因）。
- 两个标签在**引擎侧都落 `ExitReason::StopLoss`**，账本可区分，不会混进 `StrategySignal`。下单时若无 fresh best bid，内核直接 `continue`（`"strategy exit withheld: no fresh best bid"`），不挂卖单。

---

## 2. 问题 1：止损到底有没有真的成交过？

**有，成交过，但极少。**

| 场景 | 依据 | 结果 |
|---|---|---|
| polysimulator 场 `stop_loss` 成交 | `trades.jsonl` | **2 笔**，均亏损，净 -$0.65（WR 0%） |
| 相邻会话 `stop_loss` 卖单成交 | `orders.jsonl` `internalKey` 含 `:stop_loss` | **12 笔卖单全部 FILLED**（default / polysimulator 两个账户，XRP up / SOL up） |

`stop_loss` 成交的两笔明细（polysimulator）：

| id | asset | 方向 | 入场 | 出场 | 净 PnL | 净 PnL% |
|---|---|---|---|---|---|---|
| hft-438 | ETH | down | 0.66 | 0.15 | -$0.512 | -77.6% |
| hft-435 | SOL | up | 0.47 | 0.41 | -$0.135 | -14.3% |

**结论：** 二元合约的止损「以 bid 卖出」在有买盘时是**真实可成交的**（两笔卖单均以 taker 成交，`wasMakerExit=false`）。但它是**最后手段**，整个回测窗口里只触发了 2 笔；主力出场是 `take_profit`（346 笔，100% WR）和 `trailing_stop`（413 笔，94.2% WR）。止损对整体盈亏的贡献极小（-$0.65，占总净 +$27.06 的一小部分）。

---

## 3. 问题 2：被阻断的 `stop_blocked_no_liquidity` 订单最后怎么了？

**它根本不会产生订单。** `consider_exit` 在「无 fresh best bid」时即产出 `stop_blocked_no_liquidity` 意图并返回；内核侧 `exit_jobs_from_strategy_intents` 因 `strategy_signal_sell_bid` 返回 `None` 而 `continue`，不挂卖单、不成交、不扣费。持仓**保留，继续持有**，直至：
- 盘口恢复买盘后由后续评估出场（止盈/追踪/再次止损）；或
- 轮次结束，进入**结算 / unverifiable_expiry** 兜底。

可观测的“未能由策略主动出场”的持仓归宿（polysimulator）：

| 归宿 | 笔数 | WR | 净 PnL | 备注 |
|---|---|---|---|---|
| `settlement` | 19 | 89.5% | +$4.46 | 轮到期正常赎回 |
| `unverifiable_expiry` | 218 | 0.0% | -$107.38 | 场从未裁决，按兜底价平仓（记账产物，见附录） |

所以「被阻断的止损」并不会直接导致「归零」：没有买盘时止损单不成立、不下单、仓位继续持有，最终走的是**结算（回本/赎回）**或**兜底价记亏**两条路，其中 `unverifiable_expiry` 的 -$107.38 是 dry 场“无裁决→兜底价”的记账口径问题（参考 `docs/reports/lua-momentum-attribution-1109.md`），而非策略连续看错方向。

---

## 4. 问题 3：止损幅度与 RR / 期望的联合曲线

### 4.1 实际表现（当前参数 down=50 / up=30）

全部出场族的真实分布（polysimulator，1060 笔）：

| reason | n | WR | 净 PnL | PF |
|---|---|---|---|---|
| take_profit | 346 | 100.0% | +$135.68 | — |
| trailing_stop | 413 | 94.2% | +$38.93 | 28.42 |
| settlement | 19 | 89.5% | +$4.46 | 3.28 |
| stop_loss | 2 | 0.0% | -$0.65 | 0.00 |
| strategy_signal | 42 | 0.0% | -$40.25 | 0.00 |
| thesis_death_exit | 19 | 0.0% | -$3.16 | 0.00 |
| unverifiable_expiry | 218 | 0.0% | -$107.38 | 0.00 |
| manual | 1 | 0.0% | -$0.57 | 0.00 |
| **合计** | **1060** | **70.9%** | **+$27.06** | **1.17** |

方向拆分：down 545 笔 WR 73.9% / net +$29.44；up 515 笔 WR 67.8% / net -$2.38。up 腿目前略负，但已为正贡献（与旧参数下的 up 腿全灭不同），不建议砍。

### 4.2 模拟曲线（止损幅度 30 / 40 / 50 / 70）

用每笔成交的 `lowPnlPct`/`highPnlPct` 做敏感度模拟：若持仓期内净 PnL 曾跌破止损阈值，则将该笔按 `-stop%` 的成本截断；否则保持实际结果。**统计局限：** 只有高/低点，无 tick 级顺序，无法区分「跌破止损后继续深跌（止损受益）」与「跌破止损后反弹（止损止住反而更亏，即 whipsaw）」，因此曲线应理解为**方向性敏感度，而非精确预测**。下表为“仅对亏损笔做截断（fairer）”口径：

**统一止损（同时作用于 up/down 两腿）**

| stop_loss_pct | WR | 净 PnL | PF | RR | 期望/笔 |
|---|---|---|---|---|---|
| 30% | 70.9% | +$102.26 | 2.28 | 0.93 | +$0.0965 |
| 40% | 70.9% | +$79.35 | 1.77 | 0.72 | +$0.0749 |
| 50% | 70.9% | +$57.30 | 1.46 | 0.60 | +$0.0541 |
| 70% | 70.9% | +$24.91 | 1.16 | 0.47 | +$0.0235 |

**分层止损（down 腿 = S，up 腿固定 30%，贴近当前设计）**

| down 腿 stop | up 腿 stop | WR | 净 PnL | PF | RR | 期望/笔 |
|---|---|---|---|---|---|---|
| 30% | 30% | 70.9% | +$102.26 | 2.28 | 0.93 | +$0.0965 |
| 40% | 30% | 70.9% | +$91.69 | 2.01 | 0.82 | +$0.0865 |
| 50% | 30% | 70.9% | +$80.92 | 1.80 | 0.74 | +$0.0763 |
| 70% | 30% | 70.9% | +$65.09 | 1.55 | 0.64 | +$0.0614 |

**对照：当前已实现（down=50 / up=30）** — 净 +$27.06，WR 70.9%。与模拟的差距在于：真实止损几乎不触发（2/1060），绝大多数仓位在触及止损前已由 `take_profit` / `trailing_stop` 银行，或持有到结算。

### 4.3 解读

- 止损幅度收紧会**提高 RR 与期望**（模拟口径下），因为亏损被更早截断；但 WR 本身不变（胜仗不受止损影响，只在模拟中被截断的亏损笔变少）。
- 然而当前下腿止损已设为 **50%**（很宽），作用是**尾部容错（containment）**，而非主动止损；真实成交仅 2 笔，说明止盈/追踪止已覆盖了绝大部分出场。
- **不建议在此阶段改 `stop_loss_pct`**（操作员指令：先测止损真实成交率，不先改参数）。数据表明止损机制本身可成交、无阻断导致的直接归零，阻断后的仓位走结算/兜底，风险可控。

---

## 5. 附录：数据口径与局限

1. 统计基于 `data/trades/trades.jsonl` 当前快照（`strategy=lua_momentum, venue=polysimulator`，1060 笔）。与 `docs/reports/lua-momentum-attribution-1109.md`（1017 笔）存在样本口径差异（不同快照/是否剔除 fallback），两者定性一致（stop_loss 极少、未成交的止损不下单）。
2. `stop_blocked_no_liquidity` **不产生成交记录**（不下单），故不出现在 `trades.jsonl`；其数量无法从成交日志直接计数，只能从「持仓未被主动出场、最终走 settlement / unverifiable_expiry」侧反推。
3. 模拟曲线受限于高/低点聚合，不具备 tick 级逐笔还原能力，whipsaw 方向未知；结论应以「实际成交」为准。
4. `unverifiable_expiry` 的 -$107.38 / 0% WR 是 dry 场“无裁决→兜底价平仓”的记账产物，非策略失效（详见归因报告 §“为什么 -$107 不是『策略亏了』”）。
5. 引擎标签：`stop_loss` 与 `stop_blocked_no_liquidity` 均已由 `exit_reason_from_strategy_tag` 映射为 `ExitReason::StopLoss`（#1072），未落 `StrategySignal`。

---

## 6. 结论摘要（三问）

1. **止损有没有真的成交过？** 有。polysimulator 场 2 笔 `stop_loss` 成交（均亏，净 -$0.65）；相邻会话 `orders.jsonl` 中 12 笔 `:stop_loss` 卖单全部 FILLED。有买盘时止损以 bid 卖出是真实可成交的；但它是最后手段，全窗口仅 2 笔，主出场是止盈/追踪止。
2. **被阻断的 `stop_blocked_no_liquidity` 订单最后怎么了？** 不下单、不成交、仓位保留；最终走结算（19 笔，+$4.46，WR 89%）或 unverifiable_expiry 兜底（218 笔，-$107.38，记账口径产物）。不存在“因无买盘而直接归零”的止损单。
3. **止损幅度与 RR / 期望的联合？** 模拟表明收紧止损会提高 RR 与每笔期望（30% 下腿模拟期望 +$0.0965/笔 vs 70% +$0.0235/笔），但当前 down=50/up=30 已是宽容的尾部容错，真实止损触发极罕，对盈亏贡献极小。建议维持现参数，继续观察真实成交率。

---

**操作脚本：** `/tmp/stop_loss_audit/analyze.py`（只读，未入库）
