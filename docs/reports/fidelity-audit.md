# 保真审计：回放、PolySimulator，以及 lua_momentum 的 +31.6%

- 日期：2026-11-02
- 对象：`lua_momentum` 在 PolySimulator 账户 @QuickKoala2244（Default API）上的权益曲线，对照内核回放 census（`docs/reports/evolution/RETEST-2026-10-10.md`）和当前成交模型。
- 本地订单日志：该次运行没有留下。订单记录缺陷已在主干修复，见下文，本次不重写那条路径。

## 结论

**+31.6% 不能用来否定 −$17.6k，−$17.6k 也不能用来否定 +31.6%。两边量的不是同一件事。**

网站上的 +31.62 美元是模拟账户权益（现金 111.62 + 两笔仍开着的仓 20.00），不是一组成交日志。回放的 −17,575.78 美元是另一段语料、另一种策略形态、另一套费用和滑点、另一档仓位。仓库里没有 2026-10-24～28 的盘口档案，这次无法把那四天逐笔重放。PolySimulator 的历史接口要 `POLYSIM_API_KEY`，仓库和环境里都没有，未发出带密钥的请求。

方向：

| 数字 | 相对真实 Polymarket CLOB | 相对对方 |
|---|---|---|
| PolySim +31.6% | **偏乐观**（手续费为 0、可立即成交、不计队列；权益含未平仓市值） | 不能当回放过严的证据 |
| 回放 −$17.6k | **偏严**（官方 taker 费 + 已穿越卖价后再加 1 tick；10 股；双腿追价） | 不能当 PolySim 那四天失败的证据 |
| `docs/strategy-review-0.4.1.md` 的 PF≈1.24 | 那是 pair / single_leg 在 8–9 月语料上、官方费 + 1 tick 之后的数字，**不是** lua_momentum，也不是这四天 | 生产结论不要用这次 +31.6% 改写 |
| PR #869 的扣费 NO-GO | 80 个中位盘口、taker+1 tick，而且明确不是 v1.3 maker 路径 | 作为「这 80 轮没有扣费后的正期望证据」成立；作为「任何策略都没有正期望」不成立 |

## 1. 网站数字在说什么

截图（2026-11-02）：

| 项 | 值 |
|---|---|
| 起点 | $100 |
| 账户权益 | $131.62 |
| 总盈亏 | +$31.62（+31.6%） |
| 可用现金 | $111.62 |
| 持仓 | $20.00，2 笔，均为 lua_momentum |
| 胜率 | 39% |
| 曲线 | 约 10-15 至 10-24 持平，10-25～28（上海）拉升 |

恒等式：`111.62 + 20.00 = 131.62`，`131.62 − 100 = 31.62`。

所以 +31.6% **包含那 20 美元的持仓计价**。持仓若按成本计价，未实现为 0、已实现为 +31.62，同时还有 20 美元本金没回来。持仓若按市值高于成本，则 +31.62 里有一块未实现。截图分不开这两块。适配器也分不开：`BalanceBody` 有 `unrealized_pnl` 和 `total_value`（`extensions/polysimulator/src/types.rs:128-130`），执行器播种只用 `balance`（`executor.rs:167-168`），从不读 `/v1/account/positions`。内核账上的现金会是 111.62，不是网站上的 131.62。差 20 美元，也就是这 31.6 个百分点里的 20 个点。

39% 胜率配正收益，和二元合约的便宜入场一致：买在价格 `p`，赢拿到 `1−p`，输输掉 `p`，盈亏平衡胜率就是 `p`。平均入场低于 0.39 且拿到结算，39% 可以是正的。这和回放里「结算赢的只有 +1,993、结算输的 −30,892」不是同一种赔率形状。

## 2. 本地为什么没有订单

已在主干修好，本次不改修复本身。

根因有两段，都在 2026-10-26 落地：

1. `423327d`（#632，2026-10-26 08:11 +0800）`fix(polysimulator): 成交归属 FILLED 行反查`。
   场地 `GET /v1/data/trades` 的 `taker_order_id` 等于成交自己的 id，`maker_orders` 为空，`our_leg` 永远对不上（`extensions/polysimulator/src/executor.rs:361-382`，`rest.rs:256-261`）。成交被丢掉，已成交订单在未成交列表里消失，对账把它当幽灵强平。网站有盈利，界面零订单。
2. `b999a95`（#636，2026-10-26 17:34 +0800）`fix(poly): 成交归属/僵尸单/结算/时间戳四根因修复`。
   `known_venue_ids` 保留终态单的场地 id（`core/blitzkrieg_core/src/ome.rs:625-638`），否则强平之后的成交永远对不上。对账打上的终态在同一轮写入 `orders.jsonl`（`service/accounts/reconcile.rs:381-408`），否则重启把已死订单读回 LIVE 再强平一次。

已有回归：

- `extensions/polysimulator/tests/executor_live.rs` `filled_order_rows_book_our_fills_when_trade_legs_cannot`：交易行对不上时，FILLED 行仍入账（限价 0.37、成交 0.365）。
- `ome/tests.rs` `known_venue_ids_include_terminal_orders`。
- `service/tests/tests_venue_lifecycle.rs` `sweep_marked_terminal_orders_are_persisted`：幽灵强平写入 `orders.jsonl`。

缺的是正向那条：订单不在未成交列表里，但快照里有它的成交。这次补了 `a_fill_absent_from_the_open_list_is_persisted_as_filled`。不写这条，日志会停在 LIVE 或被打成 ForceClosed，看起来就像「没下过单」。

那次运行的历史补不回来。修复只对之后的成交生效。

## 3. PolySim 历史：密钥不在仓库

`GET https://api.polysimulator.com/v1/account/balance` 无密钥返回 HTTP 401 `MISSING_AUTH`（要 `X-API-Key` 或 Bearer）。`POLYSIM_API_KEY` 在本环境未设置，`.env.example:348` 是空注释，仓库里没有密钥文件。没有发带密钥的请求。

恢复命令（密钥只放环境变量，脚本不打印它）：

```sh
POLYSIM_API_KEY=… node scripts/polysim-history.mjs
POLYSIM_API_KEY=… node scripts/polysim-history.mjs --from 2026-10-24 --to 2026-10-29
node scripts/polysim-history.mjs --self-test
```

窗口按上海日历日，`--to` 不含当天。默认就是 10-24 00:00 到 10-29 00:00（上海）。脚本会拉余额、持仓、`/v1/orders?status=FILLED` 和 `/v1/data/trades`，并翻 `next_cursor`。适配器自己的扫单不翻页，两处都是 `limit=100`（`rest.rs:262-279`），所以长历史会从交易进程的视野里掉出去；恢复要靠这个脚本，不要靠扫单。

拉下来之后看三件事：`fee_rate_bps` 是不是 0、两笔未平仓的成本相对市值、窗口内 FILLED 的价格分布是不是集中在 0.39 以下。

## 4. 三套成交模型

### PolySimulator（网站盈亏的来源）

- 可成交的单在下单应答里就是 `status:"matched"`（`rest.rs:209-212`，`types.rs:219-221`）。没有队列，没有往返延迟。
- 实测成交行 `fee_rate_bps: 0`（`types.rs:451`，执行器测试夹具同样写 0）。适配器读了这个字段，记账时不用它（`executor.rs:393-427` 不看 `fee_rate_bps`）。
- FILLED 合成路径把每笔都标成 taker（`executor.rs:587` `maker: Some(false)`）。内核于是按 taker 收费，即使挂单被打到。相对网站，本地账更严；相对真实 CLOB，网站更松。
- 测试钉过价格改善：限价 0.37，成交 0.365（`executor_live.rs` 的 FILLED 夹具）。不是「总在最差档」。
- 结算只在 updown 行 `resolved` 且价格恰好是 0 或 1 时回答（`executor.rs:451-470`）。未结算的仓留在账上，网站把它们算进权益。

### 内核回放（−$17.6k 的来源）

`docs/reports/evolution/RETEST-2026-10-10.md`：

- 引擎 `f678296c`，语料是 @almach 2026-08-05～09-30 的 15 分钟事件，不是 10 月。
- `--fee-model official --slippage-ticks 1`，10 股固定，仓位上限 10000。
- lua_momentum：120,893 笔成交，20,038 笔平仓，胜率 51.16%，PF 0.433，净 **−17,575.78**。
- 其中结算 17,056 笔 **−18,899.46**（结算亏损 −30,892，结算盈利 +1,993），止盈 2,223 笔 +1,439。
- 当时的包是双腿 taker。2026-11-01 的 #849 才删掉 UP 腿并改成 maker。README 里 10-24～26 的拆分是：DOWN 44 笔胜率 56.8% **+$22.79**，UP 40 笔胜率 7.5% **−$40.37**。两腿合计是负的。网站 +31.62 含 10-25～28 的后半段和 20 美元未平仓，不能和这两腿的小样本划等号。

滑点是加在已经走过对手盘的 VWAP 上的额外 tick（`sim.rs:271-280`，`admission.rs` 里 `walk_marketable` 之后再 `apply_slippage`）。`--slippage-ticks 1` 对 taker 买入是「卖价再加 1 分」，不是「用 1 tick 代替点差」。方向：**偏严**，大约每腿 0.01 美元/股，10 股就是 0.10 美元，再加费。官方费 `0.07·p·(1−p)` 在 `exit_policy.rs:477-487`，公式是每股而不是名义本金乘费率；默认档却是更便宜的 `legacy_quadratic`（`0.125·(p·(1−p))²`）。census 显式用了 official，所以 −$17.6k 不是「费公式算错成名义本金」造成的。

Maker 默认成交概率和深度份额都是 100%（`sim.rs:243-252`）。价格碰到就按可见深度吃满，不排队列。这条对 maker 回放是**偏乐观**。census 当时走 taker，这条乐观没有进 −$17.6k。

### 真实 Polymarket CLOB

加密 taker 费是 `shares · 0.07 · p · (1−p)`，maker 费为 0，maker 返佣约 taker 费池的 20%（内核在 `--maker-rebate` 时用 `0.20 · fee_per_share · shares` 近似，默认关，`orders.rs:719-724`）。Post-only 碰到对手价会被拒（#464）。队列位置决定挂单是否成交。这些 PolySim 都没有。

## 5. 便宜入场有没有被回放做错

回放的结算美元形状是「输的名义远大于赢的」：−30,892 对 +1,993。那不是「常输小钱、偶赢 1 美元」的彩票。UP 腿在后来的实测里 0.40 以上 17 笔全灭，会把平均入场抬到盈亏平衡以上。10 股把每一笔放大。官方费在 p=0.5 是每股 0.0175（价格的 3.5%），README 写 taker 费吃掉 DOWN 腿毛利的 17.8%。PolySim 的 `fee_rate_bps = 0` 把这块抹掉了。

因此：

- 若那四天的 FILLED 均价明显低于 0.39、且大量拿到 1 美元结算，39% 胜率配正收益在零费模拟器上说得通。这是模拟器乐观，不是回放把低价合约算成归零。
- 回放把大量中高价裸腿算到结算归零，方向是真实的二元赔付（赢家 1、输家 0），不是「未结算一律记总亏损」。未平仓不进回放的已实现曲线（`backtest.rs` 的 `trade_stats` 只加已平仓）。PolySim 网站则把未平仓算进 +31.6%。这是两边口径相反的地方：回放漏掉未实现，网站把它算进收益。

## 6. 更早的几条假设（方向）

| 假设 | 代码里的事实 | 方向 |
|---|---|---|
| 回放 maker 一触即成、不排队 | 默认 `maker_fill_prob_bps = 10000`、`maker_depth_share_bps = 10000`（`sim.rs:248-249`） | 对 maker 回放偏乐观。−$17.6k 是 taker census，不受这条抬升 |
| 1 tick 滑点叠在已付的卖价上 | `walk_marketable` 的 VWAP 再 `apply_slippage` | 对带 `--slippage-ticks 1` 的 taker 回放偏严 |
| 费按名义本金而不是 `shares·0.07·p·(1−p)` | `fee_per_share` 再乘股数，official 曲线是对的 | 不是 −$17.6k 的原因 |
| 默认费比官方 crypto 费便宜 | 默认 `legacy_quadratic` | 不传 `--fee-model official` 时偏乐观。census 传了 |
| maker 返佣默认关 | `model_maker_rebate: false` | 对 maker 回放偏严，量级约 taker 费的 20% |
| pair 策略用 taker 费去卡 maker 入场 | `pair_discount_arb/strategy.lua:539-546` 在 `entry_maker` 下仍把两腿 ask 的 taker 费加进 `max_pair_cost` | 少开火，偏严。80 轮中位盘上即使不加费，ask 之和也没有低于 0.985 的轮（PR #869） |
| 名义深度 1.0 | `marketdata.rs:89` 的 top 更新把数量写成 1 | 吃单超过 1 股会被 FOK 拒绝，偏严；与 10 股 census 的成交量有关 |
| 80 轮 / 中位盘 | PR #869 四个一小时窗口各 20 轮，取 ask 之和的中位数 | 藏住瞬时折价，偏严。样本太短，不能当策略已被证伪 |
| 10-24～28 逐笔重放 | 仓库盘口只有 9 月 19–20 日的冻结窗 | 做不到。不能把 −$17.6k 安到这四天上 |

## 7. 该信哪些旧结论

- **信**：#632/#636 之后，PolySim 上新的成交会进 `orders.jsonl`。10-24～28 那次的本地记录找不回来。
- **信**：−$17.6k 描述的是「双腿 taker + 官方费 + 1 tick + 10 股 + 8–9 月 almach top 档」的结果，结算归零是大头。不要把它读成「lua_momentum 在 PolySim 上不可能赚 30%」。
- **不要信**：+31.6% 是扣过真实 CLOB 费用、排过队、并且只含已平仓的期望。它含 20 美元未平仓，场地费率为 0，成交可以在应答里瞬间完成。
- **pair / single_leg 的 PF≈1.24**（`docs/strategy-review-0.4.1.md`）是另一条策略、另一段语料。v1.3 已改 maker 报价，旧 PF 不是当前包的重跑。PR #869 拒绝把「两条腿都假设在报价上成交」写成优势，这个拒绝是对的；它的 taker+1 tick 全样本亏损是一条当前 maker 包不走的成本上限。
