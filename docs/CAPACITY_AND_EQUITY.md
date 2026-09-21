# CAPACITY_AND_EQUITY — 容量、自我冲击与权益曲线口径（#192）

> 状态：容量门禁与权益/回撤门禁**已实现并在本文记录实测数字**；复利/加仓边界
> 已按内核真实旋钮对齐。**未覆盖项在 §6 明确列出**（真实盘口快照、复利回放、
> 真实 trade log 的 CI 接入等）。
>
> 本文只描述与门禁同源的数字：每一个数都能由 §1 的两条命令复现，或由内核源码的
> 具名常量给出。凡未实测的，一律写「未覆盖」，不写估计值。

## 1. 两条门禁

```bash
# 容量与自我冲击曲线（默认 fixture 阶梯；--book 可换成抓下来的真实盘口）
node scripts/capacity-check.mjs
node scripts/capacity-check.mjs --book /tmp/ladder.json --max-slippage-bps 50 \
     --max-order-notional 6 --max-shares 10 --json /tmp/capacity.json

# 权益曲线/回撤门禁：真实日志
node scripts/equity-drawdown-check.mjs --trades data/trades/trades.jsonl \
     --initial-balance 6 --max-drawdown-pct 25 --min-trades 30 \
     --capacity /tmp/capacity.json --json /tmp/equity.json

# 权益门禁的数学与判决自检（CI 里跑的就是这条）
node scripts/equity-drawdown-check.mjs --self-test
```

两条脚本都是**只读**：读 trade log（和可选的容量报告），打印，退出。
容量门禁会拉起一个**临时 dry core**（私有 socket + scratch cwd，见
`scripts/lib/core-socket.mjs` / `child-guard.mjs`），不碰生产 socket 与 `data/`。
两条都先做 `#172` 的来源校验：被测二进制必须是 git HEAD 的修订，`core.ready`
必须报同一个 commit（见 `scripts/lib/core-provenance.mjs`）。

## 2. 自我冲击曲线（fixture 阶梯实测）

阶梯（`capacity-check.mjs` 的 `DEFAULT_LADDER`，逐档喂进内核的真实撮合）：

| 档位（价 × 股） | 0.40×10 | 0.41×20 | 0.42×50 | 0.44×100 | 0.50×500 |
|---|---|---|---|---|---|
| 累计 | 10 股 / 4.00 USD | 30 / 12.20 | 80 / 33.20 | 180 / 77.20 | 680 / 327.20 |

实测曲线（best ask 0.40，taker 买，限价 1.0，FOK 走单）：

| 下单量 | 结果 | 成交 | VWAP | 相对 best 的滑点 | 名义额 |
|---|---|---|---|---|---|
| 1 股 | FILLED | 1 | 0.400 | 0 bps | 0.40 |
| 2 股 | FILLED | 2 | 0.400 | 0 bps | 0.80 |
| 5 股 | FILLED | 5 | 0.400 | 0 bps | 2.00 |
| **10 股** | FILLED | 10 | **0.400** | **0 bps** | **4.00** |
| 20 股 | FILLED | 20 | 0.405 | 125 bps | 8.10 |
| 50 股 | FILLED | 50 | 0.412 | 300 bps | 20.60 |
| 100 股 | FILLED | 100 | 0.420 | 500 bps | 42.00 |
| 690 股（> 全簿 680） | **REJECTED** | 0 | — | — | — |

读法与门禁断言：

* **容量 = 10 股 = 4.00 USD**（在 100 bps 冲击预算内），因为第一档只有 10 股：
  再大的单必须吃 0.41、0.42…，滑点立刻跳到百 bps 级。
* 曲线**单调**（更大的单从不付更少）且**最优档内零冲击**——两条都是脚本里的断言，
  不是描述。
* 成交账必须等于「走过的 VWAP × 数量 + 该价的费」：脚本逐档比对 ledger 余额变动，
  所以曲线不是事件流的自述，而是账本的事实。
* **超全簿的单被拒绝，而不是部分成交**（690 股 → REJECTED，无 FILL 事件）。
  这正是 #171 的诚实走单：允许部分成交会把上面每一行的「容量」变成一本从未存在过的
  簿子的容量。

## 3. 成本结构：费用是已建模成本，冲击在小尺寸下为 0

内核自己声明费率模型，`#182` 已把它钉死（三个门禁都会在默认值被改动时变红）：

```
model=legacy_quadratic rate=0.125 exponent=2
feePerShare(0.40) = 0.125 × (0.40 × 0.60)² = 0.0072 USD/股（= 该价格的 1.8%）
往返（0.40 进 / 0.60 出）= 0.014400 USD/股
```

（两处口径在源码里是同一表达式的两种写法：收费走
`exit_policy::taker_fee_pct`（`0.125×(p(1-p))²/p×100` 个百分点），声明走
`service::declared_fee_per_share`（`rate×(p(1-p))^exp`）；`core.feeQuote` 的
`modelMatches` 比较两者，`#182` 的门禁把默认模型名钉死。）

即：**10 股往返的费 ≈ 0.144 USD**，而同样 10 股的自我冲击为 **0 USD**（仍在最优档内）。
费用与冲击的量级关系在 §5 的加仓讨论里是决定性的：把小账户做大，先咬人的是深度
（冲击），不是费。

## 4. 与 `--size-pct`、`--max-order-notional(-pct)`、`min/max-shares` 对齐

内核的下单量公式（`engine.rs::compute_shares`，源码即口径）：

```text
# 绝对路径（`--size-pct` 未配置；0 = 关闭，现役部署跑的就是这条，逐字节不变）
raw    = round(size_usd / price)
shares = clamp(raw, min_shares, max_shares)      # size_usd = 0 则该腿直接为 0

# 权益相对路径（#202；`--size-pct k` 且 k > 0 时 REPLACES size_usd）
budget = 余额 × k% × 该腿权重（--strategy-limit 的第 8 段）
shares = min(floor(budget / price), max_shares)  # min_shares 不再抬升仓位
                                                # budget 买不到 1 股 → 该信号不发单（计数可见）
```

风险层再叠加两道硬上限：绝对的 `max_order_notional`（保留）与**相对的**
`--max-order-notional-pct k`（#202 新增，k = 余额的百分之几）。两者都是
`orders.place` 之前**拒绝**、从不截断；相对上限对**平仓/减仓单永久豁免**
（复用 `closes_exposure` 这一处判定，与 kill switch 同源——否则 #174 的逃生通道会换个门重开）。
于是**单笔可成交量**是这些旋钮的交集：

```text
shares_max = min(max_shares, floor(max_order_notional / price), floor(余额 × k% / price))
```

各套配置在 0.40 价格下的折算，以及**这套配置在什么资金量级上才是合理的**：

| 配置 | size_usd | min/max shares | max_order_notional | 0.40 下可下单 | **该配置适用的资金量级** |
|---|---|---|---|---|---|
| 内核内置默认（`engine.rs`） | 2.5 | 10 / 10 | 100（`main.rs`） | 10 股 | **≥ 40 USD**：单笔 10 股 = 4.00–10.00 USD（0.40–1.00 成交价），即余额的 ≤ 10% |
| 面板/supervisor 默认（`supervisor.rs`） | — | 10 / 10 | **6.00** = `max(max_shares×0.6, 6)` | 10 股 | **≥ 24 USD**：单笔 4.00–6.00 USD ≤ 余额的 25% |
| README 部署示例 | — | 10 / 10 | 6 | 10 股 | 同上（≥ 24 USD） |
| **现役 dry 部署（实测 4.8 USDC）** | — | 10 / 10 | 6.00 | 10 股 | **不适用**：单笔 4.00–6.00 USD = 余额的 **83%–125%**（中位 4.00 = 83%），6.00 的「上限」本身比整个账户还大 |

上表最后一行的实测来源：现役账本 290 笔成交**全部是 10 股**（`min_shares = max_shares = 10`
把尺寸钉死，没有任何比 10 股更小的档位），`costUsd` 最小 1.00、中位 4.00、最大 4.50 USD；
按 0.40–0.60 的成交价区间折算即 4.00–6.00 USD。**最差一笔 −3.4736 USD = 现役 4.8 USD 账户的 72%**
（下单时点的余额更低，占比更高）。也就是说：在现役资金下，`--max-order-notional 6.00`
是一个**永远不会触发的装饰**——它和最小下单量是同一个数。

`--size-pct` / `--max-order-notional-pct` 的实测口径（现役 4.8 USD，k = 20%）：

```bash
# 一条命令：现役资金下「任何一笔的最大可能损失」是否 ≤ 余额 × k
node scripts/risk-sizing-check.mjs --balance 4.8 --size-pct 20 --max-order-notional-pct 20
```

```text
risk-sizing: account 4.8 USD, per-order cap 20% = 0.96 USD, per-entry budget 20% of equity
  kernel: share band 10 shares × 1 = 10 USD; equity cap 0.96; worst case one order 0.96 USD = 20.0% of equity
  ok   a close/reduce is exempt from the equity cap (the escape hatch #174 depends on)
  ok   an over-cap entry (the 10-share live ticket): rejected — RiskRejected: notional 4.0 exceeds the 20% equity cap 0.960 (equity 4.8); the cap never truncates
  ok   the largest in-cap entry (2 shares): admitted (filled 2)
  ok   a close over the cap (the way out): admitted (filled 2)
  ok   a non-close sell over the cap: rejected — RiskRejected: notional 1.18 exceeds the 20% equity cap …
  verdict: worst one-order commitment 0.96 USD vs bound 0.96 USD (20% of 4.8)
```

把上限摘掉（`--max-order-notional-pct 0`，也即**今天的生产姿态**——supervisor 只传绝对的
`--max-order-notional 6.00`，从来没人给过相对上限），同一条命令报出的是**现存的洞**而不是
绿色通过（exit 1）：

```text
  FAIL an over-cap entry (the 10-share live ticket): admitted (filled 10)
  FAIL a non-close sell over the cap: admitted (filled 1)
  verdict: worst one-order commitment 0.96 USD vs bound 0 USD (0% of 4.8)
  FAIL verdict — no per-order bound as a share of the account (--max-order-notional-pct 0)
RISK-SIZING FAILED (5)
```

未配置时的面板数字（`engine.stats.sizing`，见 `tests/risk_gates.rs` 的断言）是
`worstCaseOrderUsd = 10.00`、`worstCasePctOfEquity = 208.333`、`equityCapUsd = null`——
即「一笔可以押上整个账户的 2 倍」。

要点（这四条是「复利/加仓边界」的全部）：

1. **`size_usd` 在 [min_shares, max_shares] 内不生效**。默认 min=max=10，于是
   `size_usd` 只有 `round(size_usd/price) ≥ 10`（即 `size_usd ≥ 10 × price`）才有意义；
   低于它时永远是 10 股。**改 `size_usd` 不会加仓**，改的是 `min/max_shares`。
   要让尺寸随账户走，用 `--size-pct`（余额 × k%）——它**取代** `size_usd`，
   并且**不理会 `min_shares`**（份额下限把仓位抬到预算之上，正是 #202 本身）。
2. **`max_order_notional` 是价格上限，不是尺寸旋钮**。`max_shares = 10` 时 6.00 USD
   等价于「价格 ≤ 0.60 才下得出去」；10 股 × 0.70 = 7.00 > 6.00 会被风险层直接拒。
   supervisor 里 `max_shares × 0.6` 的写法就是这个含义（见源码注释：按真实最坏情况定，
   而不是按策略名义尺寸）。**绝对额在账本变化时不会跟着变**，这就是
   `--max-order-notional-pct` 存在的理由：同一个 k 在 4.8 与 480 USD 上都是同一句话。
3. **加仓 = 三个旋钮一起动 + 先测容量**。把 10 股提到 50 股，需要同时
   `--min-shares/--max-shares 50`、`--max-order-notional ≥ 50 × 0.6`（supervisor 自动
   跟随）、以及每个策略的 `--strategy-limit name:...:size_usd:...`（全局没有
   `--size-usd` 标志，dollar 预算是**按策略**给的，内置默认 2.5）。在本文的阶梯上，
   50 股要付 300 bps 的自我冲击（20.60 USD 名义额里 ~0.60 USD 是冲击成本），
   已经和往返手续费（50 股 × 0.0144 = 0.72 USD）同级——**容量决定了这个账户能长到
   多大，而不是余额**。这正是 `--require-caps-within-capacity` 存在的理由：它把
   「风险上限必须落在实测容量内」变成一条会失败的断言，而不是一句提醒。
4. **权益相对定仓会自动加仓，所以必须先看容量**。`--size-pct 20` 在 4.8 USD 上是
   0.96 USD/笔（2 股 @0.40），在 480 USD 上是 96 USD/笔（218 股 @0.40）——而 §2 的
   阶梯显示 218 股要吃掉整个 680 股示例簿的三分之一、滑点进入百 bps 级。因此
   `--require-caps-within-capacity` 与 `risk-sizing-check.mjs` 要**同时**通过：
   前者管「上限落在容量内」，后者管「单笔 ≤ 余额 × k」。

## 5. 权益曲线/回撤门禁（`equity-drawdown-check.mjs`）

指标（全部来自 `data/trades/trades.jsonl` 的 `netPnlUsd` 按 `exitTime` 排序）：

| 指标 | 定义 |
|---|---|
| equity / peak | `初始资金 + Σnet`，以及历史最高点 |
| max drawdown | peak 到其后谷底的最大绝对额与**相对峰值**的百分比 |
| current drawdown | 当前 equity 距历史峰值的回撤 |
| per-trade return | `net / 交易前 equity`（复利口径），据此得 mean、样本 sd |
| Sharpe | `mean/sd × √(年化交易数)`；年化率由 `--trades-per-year` 给出，否则取相邻 `exitTime` 间隔的**中位数**（<1s 的爆发式节奏不年化，避免造出几千的假 Sharpe） |
| profit factor / win rate / expectancy | 常规口径，用于交叉检查 |
| worst trade % | 单笔最差值 / 该笔交易前的 equity——**尺寸能不能一笔打死账户**的度量 |
| ruin | `equity <= 0`：独立于任何百分比阈值，先判 |

判决（`verdicts()`，自检与实跑共用同一函数）：

* **空日志直接失败**——空数据不是通过，这正是本仓库反复出现的「永远绿的检查」缺陷。
* max drawdown > `--max-drawdown-pct`（默认 25%）失败。
* `equity <= 0` 失败（即使把回撤预算调到无穷）。
* `--require-trades N` 时交易数不足失败。
* Sharpe 只在样本 ≥ `--min-trades`（默认 30）时强制；不足时**打印「未强制」并 continue**，
  既不假装通过、也不因样本不足误杀。

`--self-test` 用 8 组手算 fixture 证明判决仍然会响：单调上升曲线（回撤 0）、
**恰好 50% 回撤的曲线（20% 预算下必须被点名）**、归零（ruin）、空日志、
单笔/无节奏样本（Sharpe 必须报 n/a 而不是编一个数）、样本地板、
年化系数（Sharpe 严格按 √年化数缩放）、冲击归因的插值与「超出阶梯即截断并计数」。

实测样例（真实内核日志：scratch dry core 跑 5 笔 taker 往返，一胜一负交替；
`--initial-balance 100 --trades-per-year 200`）：

```
equity        99.7809 (-$0.2191, -0.22%)      peak 100.3514
max drawdown  -$0.9220 = 0.92% (at trade hft-4)
wins          3W/2L (60.00%), profit factor 0.828
fees          $0.7191 = 68.21% of gross profit
worst trade   -0.64% of the equity it was taken on
(Sharpe not enforced: 5 < 30 trades)
```

同一份日志在 `--max-drawdown-pct 0.5` 下**退出码 1** 并打印
`FAIL max drawdown 0.92% exceeds the 0.50% budget`——门禁的失败路径是在真实日志上
验证过的，不只是 fixture。

`--capacity <json>` 会追加一条**归因**（不是重放）：按每笔的股数在实测曲线上插值，
得到该笔的自我冲击；超出阶梯的尺寸截断到最后一个测点并计数。它是「一笔一条腿」的
下界，只用于回答「这笔净额里有多少可能是深度成本」，不用于预测。

## 6. 未覆盖（如实列出）

1. **真实盘口快照未接入**：容量曲线默认来自 fixture 阶梯（`--book` 可喂抓下来的
   `{"asks":[[price,size],...]}`，但本 PR 未附带任何真实快照，也没有自动抓取）。
   阶梯深度是**示例账本的深度**，不是 Polymarket 任何真实 token 的深度。
2. **没有复利回放**：本文给出的是旋钮之间的关系（§4）与前端容量（§2），
   没有「按权益百分比增长下单量」的逐笔重放，因此**没有复利下的权益曲线证据**。
3. **冲击归因是近似**：一笔一条腿、按股数在阶梯上插值、假设退出侧深度对称
   （实际只测了买侧）。往返两侧的真实冲击未被测量。
4. **Sharpe 的年化口径**：`--trades-per-year` 或中位间隔二者选一，样本 < 30 笔不强制；
   不同 cadence 下的 Sharpe 不可直接比较（脚本会打印所用 cadence）。
5. **CI 只跑 `--self-test`**：仓库里没有随代码提交的 trade log，真实日志模式属于运维动作
   （部署机上手动/定时跑）。因此 CI 证明的是「判决会响」，不是「当前账户曲线健康」。
6. **未覆盖 live 模式**：全部实测在 `dry` 模式、未授权 `live`；dry 的撮合是内核自己的
   走单模型，真实成交/排队/部分成交的差异不在本文范围。
7. **容量与轮盘/多资产无关**：一次只测一个 token 的一种尺寸；同一轮盘多个 token
   同时下单的**组合冲击**未测。

## 7. 复现清单

```bash
cd core/blitzkrieg_core && cargo build --release      # 或 worktree 根 cargo build --release
cd user_layer/strategies && cargo build --release     # 引擎场景需要真实策略 cdylib
node scripts/capacity-check.mjs --json /tmp/capacity.json
node scripts/equity-drawdown-check.mjs --trades data/trades/trades.jsonl \
     --initial-balance 6 --capacity /tmp/capacity.json
node scripts/equity-drawdown-check.mjs --self-test
```

三者均先打印被测二进制的 `--version`（`0.2.0+g<sha>`）与 `core.ready` 的 commit，
所以「这份数字测的是哪次提交」不靠记忆。
