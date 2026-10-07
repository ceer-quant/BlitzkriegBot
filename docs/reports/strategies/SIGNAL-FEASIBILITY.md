# 零信号策略可行性 + 胜率/PF 口径定义（issue #402）

> **一句话结论：5 个策略的零信号全部是数据面结构性的——v2 语料（converterVersion 2）里 spot 事件 = 0、
> book 级事件 = 0，3 个 kline 触发型策略（flash_arb / hot_side_momentum / kline_probe）因此触发器恒空，
> 2 个 spot-gate 型策略（mad_dog / market_maker）被默认 `spot_missing = block` 全量否决（#401 已确认）。
> 唤醒的第一步是转换器收编 spot 行情（manifest bump v3）；第二步是各策略把 1.5–30s 的微结构窗口
> 迁移到语料实际 30–60s+ 的 top 节拍上（全部在 #401 已声明的 knob 域内）；第三步（mad_dog 的亚秒
> panic 前提）在钱包语料框架内不可满足，需要 book 级数据或策略改造。「65% 胜率 / PF 3」在 per-strategy
> 单开口径与 portfolio 口径下都可度量，但对当前 5 策略是 0/0（不可度量）——唤醒把它们从「不可测」
> 变成「可测可调」；诚实基线是 pair 单开 v2 全量 closed 7,398 / WR 53.80% / PF 1.1126 / netPnl $87.56。**

本文是纯静态分析：读策略源码与内核代码、流式普查冻结语料（只读），不跑回放、不改代码。
所有代码引用行号基于 base 41e54cfc。

---

## 1. v2 语料事件普查（实测）

语料：`data/onchain/3725d52f…_1785888000_1790812799-15m-act-4bf06519abf5.jsonl`
（manifest：converterVersion 2，conditions 9,780，events 356,749，verdicts 38,603，cashflows 84）。
流式逐行统计（python3，只读），原始数字见 agent 工件 `bk-agentfeas/event-census.json`。

### 1.1 事件类分布（15m v2，356,749 事件）

| k | 条数 | 占比 | 语义（data_source.rs:18–25） |
|---|---:|---:|---|
| `top` | 293,286 | 82.2% | top-of-book 双边价（bb/ba，无真实深度） |
| `trade` | 45,813 | 12.8% | 钱包成交打印（信息性，engine.rs:630 不喂策略） |
| `resolution` | 8,034 | 2.3% | verdict 结算输入（#377/#378） |
| `round` | 4,766 | 1.3% | 市场声明（轮次开始） |
| `round_end` | 4,766 | 1.3% | 轮次结束 |
| `cashflow` | 84 | 0.02% | 钱包级现金（rebate 56 / reward 28，#388） |
| `spot` | **0** | — | **Binance spot 行情——不存在** |
| `book` | **0** | — | **全深度订单簿——不存在** |

round 声明的市场轮次分布：BTC 3,752 / ETH 1,819 / SOL 1,801 / DOGE 1,172 / XRP 876 / BNB 360。

### 1.2 top 节拍（决定「fresh/窗口」类门槛能否满足）

对 19,560 个 token 的逐 token 相邻事件间隔：

| 间隔 | 占比 |
|---|---:|
| < 5s | ≈0（<0.001%） |
| 5–15s | 12 条（0.004%） |
| 15–30s | 173 条（0.06%） |
| 30–60s | 102,538 条（35.0%） |
| > 60s | 171,003 条（58.3%） |

每 token 每轮 top 事件数：中位 **15**（min 9 / max 17）。即 venue 公共流在本语料窗口内的
top 节拍是 **30 秒到分钟级**，不是亚秒级。

### 1.3 5m 语料同构验证

5m 全量语料（478,583 事件）：top 280,778 / trade 168,729 / round+round_end 各 14,538，
间隔分布同构（30–60s 31% / >60s 49%），同样 **spot=0、book=0**。且该语料无 resolution/cashflow
——是 converter v1 形态（pre-verdict），结论：换 5m 市场语料**不解决**任何一类的缺口。

### 1.4 为什么语料里没有 spot/book：converter v2 的输入面

`onchain.rs` 的确定性转换器（`convert()`，onchain.rs:1425+）的输入面是：

1. 钱包 activity fills（成交行）→ trade 事件与 round 声明；
2. gamma 元数据 → token id / question / 轮次窗口；
3. token 价格历史 → round 声明的种子价 + 聚合为 top 事件；
4. 钱包 REDEEM 行 → verdicts → resolution 事件（#377/#378）；
5. MAKER_REBATE / REWARD 行 → cashflow 事件（#388，`CONVERTER_VERSION = 2`，onchain.rs:325）。

**Binance spot 行情不在输入面**——这不是数据丢失，是 v2 转换器从未消费过这个源，所以
`spot` 事件在结构上不可能出现在任何 v2 语料里。`book` 同理：钱包语料的 top 来自聚合价格
历史，不是 venue 的 L2 流。replay 端（data_source.rs:249–330 `json_to_event`）**已经支持
全部 8 类事件**——缺口完全在生产端（转换器），不在消费端（回放引擎）。

---

## 2. 引擎侧喂给路径：策略能收到什么

engine.rs `on_data`（531+）的分发决定了每个 Lua 回调的货源：

| DataEvent | 策略回调 | 备注 |
|---|---|---|
| `Book` / `TopOfBook` | `bk_on_book` + `bk_evaluate` 的 book 视图 | top 路径 `update_top` 用**名义 size 1.0**（marketdata.rs:62–82）→ 快照的 `bid_depth/ask_depth/obi` 从名义 size 派生（strategy_logic/model.rs:81–93），**不携带真实深度** |
| `Spot` | **不直接回调**——进 spot buffer 并喂 kline 聚合器（engine.rs:569–578） | sec1 bar 闭合时以 `k.symbol = asset`（"BTC"…）dispatch `bk_on_kline` |
| Book/Top 的 mid | 同一聚合器按 **token id** 聚合（`feed_klines`，engine.rs:659–666） | token 键的 bar 不会被任何 `m.asset` 查询读到（flash_arb 源码注释明说了这个设计） |
| `RoundMarkets` | `bk_on_round` | |
| `Resolution` | 不喂策略——结算输入（engine.rs:633–637） | |
| `Cashflow` | 不喂策略、不进 netPnl——ledger 输入（engine.rs:639–643） | |
| `Trade` | 不喂策略——信息性（engine.rs:630–634） | |

聚合器默认区间含 sec1（aggregator.rs:67–76，`DEFAULT_INTERVALS` 9 档），所以**只要 spot 事件
存在**，`bk_on_kline`（interval=="sec1" && is_closed）就有货——引擎侧管道是通的，缺的只是源。

两个次生的引擎常量直接影响门槛评估：

- **fresh 判定**：`fresh_book` 要求 book 年龄 ≤ `max_orderbook_stale_ms`（engine.rs:1567–1578；
  默认 **8,000ms**，engine.rs:35；CLI 上限 30s）。top 节拍 30–60s+ ⇒ 语料里大量 evaluate 时刻
  `bk.book(tok).fresh == false`，需要 fresh book 的门槛在事件间隙里是关着的。
- **回放节拍**：tick_ms 默认 50（backtest.rs:93），事件批后必有 evaluate——评估机会不缺，
  缺的是评估时的数据新鲜度。

---

## 3. 逐策略：订阅面 / 零信号根因 / 唤醒路径 / 工作量

工作量分级词汇表：

- **仅数据面** = converter 收编新事件类（onchain.rs 输入面 + manifest bump v3 + 缓存失效语义），
  策略与内核零改动；
- **域内调参** = 只动 #401 已声明的 knob 域（shadow evolution / config），无代码；
- **需内环** = 策略逻辑或内核语义（fill model / 深度语义 / 新回调）需要代码改动。

### 3.1 flash_arb — spot→venue 滞后捕捉

- **订阅面**：`bk_on_kline`（仅 sec1 closed bar，asset 键）做触发锁存（strategy.lua:159–203）；
  `bk_evaluate` 读 round / markets / 目标腿 book（fresh、best_ask、bid_depth、obi）。
  触发后必须在 `lag_max_ms`（默认 1500ms）内完成评估。`exits` 恒空（seal）。
- **根因**：触发面 sec1 spot bars = 0 条 ⇒ `triggers` 恒空 ⇒ `entries` 恒空。fail-closed by
  construction（源码注释明说：no spot → no trigger，无 `spot_missing` 旋钮可披露）。
- **唤醒路径**：
  1. **数据面**：converter 收编 Binance spot（分钟/秒级 kline 或 trade 历史 → 转换为 spot
     事件流；manifest bump v3）。这一步之后 `bk_on_kline` 开始有货，触发器开始锁存。
  2. **域内调参**（三项都有 #401 域）：`lag_max_ms` [1000,10000] → 8000–10000（1.5s 的滞后窗
     在 30–60s 的 top 节拍下命中率 ≈ 窗宽/间隔 ≈ 3%，拉满也只有 ~20–30%，但可测）；
     `min_bid_depth` [0,5000] → 0 或个位数（top 快照 depth ≈ 名义 1.0，500 是全否决）；
     `mom_threshold_pct`/`mom_window_sec` 按唤醒后触发频率校准。
  3. **需内环的部分（可选）**：真实 bid_depth 语义（要 book 级数据）或把深度门槛语义改为
     「可从 top 快照满足」的代理。
- **判定**：数据面 + 域内调参即可达到**非零信号、可测可调**；滞后论题本身（venue 落后 spot
  多少毫秒）要在唤醒后用回放实测，本文不预判其盈利性。**工作量：数据面 + 域内（中）。**

### 3.2 hot_side_momentum — 确认领先后持有到结算

- **订阅面**：`bk_on_kline`（sec1 closed，asset 键）触发（strategy.lua:141–182）；evaluate 要求
  round 进度窗 40–70%（`round_sec` 900 可调，300–1800）、两腿 book fresh、双边 ask 严格分高下
  定 leader、leader ask ∈ [0.60, 0.80]、动量方向与 leader 对齐、触发新鲜 ≤ `mom_max_age_ms`
  （2000ms）。入场一次，持有到结算（`holds_to_settlement`），无退出管理。
- **根因**：同 flash_arb——触发面 0 条 sec1 spot bars。
- **唤醒路径**：数据面（同上）+ 域内调参，且**约束最宽松**：
  - `mom_max_age_ms` [1000,15000] → 10000–15000（15s 动量窗对应的天然新鲜度）；
  - `mom_window_sec` [5,120] → 30–60（在 30–60s 的 top 节拍下，"确认"天然以分钟为尺度）；
  - `round_sec` 已支持 300（5m 语料同样可用）。
  - 持有到结算意味着入场后不需要持续 fresh book——对 top 稀疏最不敏感的策略。
- **判定**：**五个里唤醒成本最低**（数据面 + 域内即可），也是唤醒后最先能进入 WR/PF 统计的。
  **工作量：数据面 + 域内（低）。**

### 3.3 kline_probe — 诊断探针（throwaway replay probe）

- **订阅面**：flash_arb 的锁存器原样（sec1 closed，strategy.lua:71–107）；evaluate 把深度/obi/
  avail 全部打进 reason 文本（`bisectD…`），仅保留 avail 门槛与价格带，价格固定报 "0.01"
  （strategy.lua:109–164）。
- **根因**：触发面 0。
- **唤醒路径**：**仅数据面**——它就是「kline 管道是否活了」的测量仪（`kline_stats()` 的
  `symbols_seen` / closed bar 计数 + 本策略的 order 计数与 reason 文本）。唤醒 spot 后第一个
  跑的就应该是它。
- **判定**：唤醒后的验收仪器，不是盈利策略。**工作量：仅数据面（低）。**

### 3.4 mad_dog — 恐慌插针捕捉

- **订阅面**：`bk_on_book`（mid 250ms 桶采样）驱动 dominance（mid≥0.55 持续 20s 或 60s 滚动
  均值，round 锁存）与 panic episode（mid 跌破 0.35 且 3s 内跌幅 ≥15%，strategy.lua:349–367、
  220–285）；`bk_on_kline`（sec1 closed）**只用于 spot veto**（`spot_ok`，strategy.lua:315–343）；
  evaluate 对 armed episode 挂 `wick_offset`（0.02）低于插针低点的 resting maker bid。
- **根因（双重锁死）**：
  1. **spot veto 全否决**：语料无 spot ⇒ `spot_ok` 走 `spot_missing` 默认 "block" ⇒ 每个入场
     候选被否（#401 校准标注的正是这一层）。
  2. **即使 `spot_missing=pass`（诊断模式）触发层也是死的**：panic 检测的速度窗（3s 内从 ref
     跌 ≥15%）要求窗口内存在样本——top 节拍 30–60s+ 时 3s 窗内几乎恒无样本 ⇒ episode 永不
     armed（§1.2 实测：<15s 的间隔占 0.004%）。250ms 采样环的物理前提（亚秒级价格路径）在
     本语料的公共流节拍下不存在。深度门槛 `min_bid_depth=500` vs 名义 1.0 是第三重否决。
- **唤醒路径**：
  1. 数据面（spot 收编）只解决 veto 层；
  2. 域内调参（`dip_speed_sec` [1,30]→30、`dominant_hold_sec` [5,300]→300、`min_bid_depth`→0）
     能把门槛节拍拉到 30s 尺度，但「3s 内 15% 的插针」这个策略前提本身需要亚秒级 book 路径；
  3. **需内环或不可得**：venue 公共流没有 250ms 的 L2 book（钱包语料框架内无法收编），要么
     改造策略以 30–60s 节拍重新定义「恐慌」（策略代码），要么接受它在本市场不可测。
- **判定**：唤醒成本最高；`spot_missing=pass` + spot 收编后可以先把 veto 层变成可测，
  触发层需要单独的改造决策。**工作量：数据面 + 需内环（高）。**

### 3.5 market_maker — 被动价差收割

- **订阅面**：`bk_on_book`（mid 环）驱动 `chip_calm`（30s 趋势窗：窗内 ≥2 样本且跨度 ≥29s，
  strategy.lua:237–256）；`bk_on_kline` 只做 spot veto（`spot_missing=block` 默认，
  strategy.lua:263–314）；evaluate：chip mid ∈ [0.20, 0.42]、深度 300/300、obi ≤ 0.4、
  calm+spot 双确认 → 挂 mid−0.02 的 resting bid；mid 回升 bid+0.03 建议平并 re-arm。
- **根因（三重）**：① spot veto block 全否决；② `chip_calm` 要求 30s 窗内 2 个样本（跨度
  ≥29s）——top 节拍 30–60s 时 30s 窗内通常 0–1 个样本 ⇒ fail-closed（源码注释自己写明：
  tracked <30s 无证明 → disarmed）；③ 深度 300 vs 名义 1.0。
- **唤醒路径**：
  1. 数据面（spot 收编）解 veto 层；
  2. 域内调参：`trend_window_sec` → 60–120（相邻 top 间隔 30–60s ⇒ 60s+ 窗有样本）；
     `min_bid_depth`/`min_ask_depth` [0,5000]→0（关）；`max_abs_obi` [0.05,1]→1（关）——
     三个旋钮全在 #401 域内；
  3. fill 层是后续问题：30–60s 节拍下 resting bid 的成交概率由 fill model 决定，属于唤醒后
     的校准课题，不是唤醒前提。
- **判定**：数据面 + 域内即可测。**工作量：数据面 + 域内（中）。**

---

## 4. 缺口表：策略需要的事件类 vs 语料有的事件类

| 策略 | 需要的（含默认门槛） | 语料实有 | 缺口定性 |
|---|---|---|---|
| flash_arb | `spot`（sec1 bars，asset 键）触发；fresh book；真实深度 | spot=0；top 30–60s+；depth≈名义 1.0 | **触发面全缺** + 新鲜度错配 + 深度门槛 |
| hot_side_momentum | `spot`（sec1 bars）触发；两腿 fresh book | spot=0；top 30–60s+ | **触发面全缺** + 新鲜度错配（域内可迁） |
| kline_probe | `spot`（sec1 bars）触发 | spot=0 | **触发面全缺**（其余门槛已开） |
| mad_dog | `spot`（veto，10s 窗）；250ms 级 mid 序列（20s/60s dominance + 3s 速度窗）；真实深度 | spot=0；top 30–60s+；depth≈1.0 | **veto 全缺 + 触发面前提不成立** + 深度门槛 |
| market_maker | `spot`（veto，30s 窗）；30s 窗内 ≥2 个 mid 样本；双向深度 300 | spot=0；top 30–60s+；depth≈1.0 | **veto 全缺 + calm 证明不可得（域内可迁）** + 深度门槛 |

对照线格式 8 类（data_source.rs:18–25）：语料有 top/trade/resolution/round/round_end/cashflow，
缺 spot/book——而 replay 消费端全支持，converter 生产端不产。**缺口 100% 在数据面生产端。**

---

## 5. 胜率 / PF 口径定义（WR/PF caliber）

### 5.1 三个口径，各自的合法用途

| 口径 | 定义 | 合法用途 |
|---|---|---|
| **A. per-strategy 单开** | 只启用一个策略的确定性回放（identity fill model 为校准基准），统计该策略的 closed trades | 单策略达标判断（「65%/PF3」的第一含义）；域内寻优的 A/B |
| **B. portfolio 多开** | 多策略同时启用（`--enable-strategy` 多值 / `data/strategy-state.json` 持久化），共享资金面、E25 仲裁、E26 熔断 | **PF 的合法上探路径**（见 5.4）；组合级达标判断（「65%/PF3」的第二含义） |
| **C. wallet-shadow 对齐** | 与 @almach 账本逐位对齐（CALIBRATION.md 的 A/B 臂） | 校准回放保真度；**不用于**策略达标判断（sizing 轴结构性残差 ~49×，CALIBRATION.md §1） |

两个口径都只在 **v2 verdict 语料** 上有效（见 5.2）。样本下限：报告 closed < ~30 时不谈
达标（spread_arb 的 PF 3.86 案例是 235 closed——作为「存在性证明」合格，作为达标宣称
样本偏小，time-split holdout 74.67%/71.76% 是它可信的原因）。

### 5.2 v2 语料特性（口径的边界）

1. **verdict 结算（#377/#378）**：converter 把钱包 REDEEM 行折叠成 per-condition verdict
   （`usdcSize>0` = 赢腿资产、零额 burn 行 = 输腿），在轮次到期时刻发 `Resolution` 事件，
   `archive_verdict` 市场的 dry 结算梯子停机——**回放不再发明结算，账面幻觉（无凭据的
   payout、无对手腿的卡死持仓）在源头上被修掉**。因此 v2 的 netPnl 口径比 v1 严：
   v1 的 PF/WR 若含 dry 结算产物，与 v2 数字**不可比**（跨语料比较必须标注 converter 版本）。
2. **cashflow（#388/#392）**：MAKER_REBATE / REWARD 每账本行一个 `cashflow` 事件，回放在
   该时刻记入**钱包级** ledger（`Ledger::credit_cashflow`），**不触碰 position/order/settlement
   语义**（engine.rs:639–643），报告单列（backtest.rs 的 `CashflowReport`：rebates/rebatesUsd/
   rewards/rewardsUsd + Display 行）。**不进 netPnl。** 本语料：rebate 56 行 $20,806.5602 +
   reward 28 行 $4,755.207（CALIBRATION.md 基线锚定）。
   **口径规则：WR/PF 一律按 trade 结算现金流计算；把 rebate 计入 PF 分子是口径作弊**
   ——它是钱包级的、与任何策略决策无关的 venue 返现，且 84 行对 7,398 closed 的 skew
   意味着它无法被归因到单一策略。
3. **fill 诚实性**：fill rate / partial-fill rate 显式报告（#183），maker 入场单有
   entry_maker_timeout；TAKER_REBATE $3,889.99 是实盘存在、语料不含的结构残差——
   回放的 maker 策略 PF 因此是**保守下界**（对 maker-only 策略不利方向已知）。

### 5.3 已知基线（引用，不复跑）

| 数字 | 口径 | 出处 |
|---|---|---|
| pair 单开 v2 全量：closed 7,398 / **WR 53.80% / PF 1.1126** / netPnl $87.56 | A，v2 verdict 语料 | CALIBRATION.md 基线锚定（与历史 fast6 治#2 终版逐位一致） |
| 单腿修订版旧口径 **PF 2.79** | v1 语料、#394 报告口径 | **与 v2 不可比**（verdict 收紧前的账面）——引用必须带此注 |
| #401 round 1 校准：**无 ≥+3% 且 val 同向的臂** ⇒ 零默认改动 | 域内寻优第一轮 | DOMAINS.md 校准段 |
| spread_arb 0.88 折扣：235 closed / WR 73.62% / payoff 1.38 / **PF 3.86** | A，9.68M 事件冻结语料 | CHANGELOG——单策略在本体系内达到过 65%+/PF3+ 的存在性证明 |

### 5.4 「要到 65% / PF 3 还差什么」：量化分解框架

记 WR = 胜率，payoff = 平均盈利额/平均亏损额，则 **PF = WR·payoff / (1−WR)**。
基线 53.80% / PF 1.1126 反解 payoff = 0.955（赢亏几乎对称，赢面略大于输面）。

**目标矩阵**（满足 65%/PF3 的合法组合）：

| 路径 | WR 要求 | payoff 要求 | 与基线的差距 |
|---|---|---|---|
| 只提 payoff | 53.80% 不变 | **2.58** | payoff ×2.70 |
| 只提 WR（payoff 0.955 不变） | **78.6%** | 0.955 | WR +24.8pp |
| 目标 WR=65% | 65.0% | **1.62** | WR +11.2pp 且 payoff ×1.70 |

三档差距对应的合法杠杆与预期量级（给区间和依据，不许拍胸脯）：

**杠杆 1：域内寻优（shadow evolution，#401 机制已就位）**
- 作用面：入场价/门槛 → 同时影响 WR 与 payoff（入场更深的折扣 → 赢时空间大、输时止损近）。
- 依据：spread_arb 0.98→0.88 案例（WR +28.7pp、payoff 1.18→1.38、PF 0.96→3.86）证明该项
  弹性巨大，但那是「入场价错配」的特例；#401 round 1 对 5 个可校准策略的 24 个探针臂全部
  低于默认 ⇒ 对**当前基线策略**的边际量级预期 **WR +0~5pp / PF +0~0.3**（区间依据：round 1
  零改进 = 当前默认点邻域平坦；spread_arb 式跃迁需要存在一个错配的默认点，9 个 knob 的
  domain 注记里没有这类已知错配）。
- 对 5 个零信号策略：唤醒后它们才有 knob 曲线，第一轮 sweep 预期就是 round 1 式的
  「确认默认或小幅修正」，不是跃迁。

**杠杆 2：组合分散（portfolio 口径，PF 的合法上探主路径）**
- 作用面：**PF 在组合层不是各成分 PF 的加权平均**——负相关/低相关策略的毛利相加、毛亏
  在时间上错开，组合 PF 数学上可超过成分 PF 的闭合均值；WR 在组合层是 closed 数加权
  平均，**组合不创造 WR**（上限 = 各策略 WR 按其 closed 数加权）。
- 量级：唤醒 5 策略后组合含 6–9 个策略；若唤醒策略（尤其 hold-to-settlement 类）WR 落在
  55–70% 域，组合 WR 可被拉动 **+1~4pp**；组合 PF 相对单策略加权均值的历史结构增益
  （依据：pair 与 single-leg 的轮次覆盖互补性，#394 的 5,391/9,780 单腿轮）预期 **×1.2~1.8**。
  到 3.0 需要成分里至少一个 PF ≥2 级别的策略与基线低相关——唤醒后由数据说话。
- 约束：共享资金面与 E25 仲裁意味着组合不是自由叠加（同 token 同向入场互斥），
  组合增益评估必须用口径 B 的实际回放，不能用成分报告相加。

**杠杆 3：数据面唤醒（本文 §3 的可执行清单）**
- 作用面：把 5 个策略从 0/0（不可度量）变成可度量——这是「65%/PF3」对它们**有意义的
  前提**，不是直接提升。唤醒后的第一份报告只回答「有没有信号、样本多少、WR/PF 落在哪」。
- 顺序建议：spot 收编（converter v3，一次解决 3 个触发型 + 2 个 veto 型）→ kline_probe 验收
  管道 → hot_side_momentum（成本最低）→ flash_arb / market_maker（域内迁移）→ mad_dog
  （触发层需单独决策）。
- 覆盖差的量化：当前可校准策略 5/10（#401）；唤醒后 9/10（oracle_ruler 是测量契约不可调）。
  组合 PF 的分子分母都会被新增 closed 样本改变，**方向不可预判**——诚实表述：唤醒把
  「差什么」从数据问题变成统计问题。

**杠杆 4：内核 sizing 面（影响收益分配，不影响 WR/PF 的比值口径）**
- CALIBRATION.md §1 已确认：sizing 是全局的（share band + 单单名义上限），策略无
  per-strategy sizing 面；live 每轮中位 $48.31 vs 回测 p50 $0.98（~49×）。sizing 放大/缩小
  净收益绝对值，**不改变 WR 与 PF**（比值口径对 sizing 不敏感）——因此它不是 65%/PF3
  的杠杆，是 $目标的杠杆。列出在此只为防止口径混淆。

**框架小结（合法上探路线图）**：
基线 53.80%/1.1126 →（杠杆 1，+0~5pp WR）→（杠杆 3 唤醒后加入新策略样本）→
（杠杆 2 组合，PF ×1.2~1.8 于成分均值）。三个杠杆全用满的诚实区间：**组合口径下
PF 1.5~2.5、WR 55~63%** 是本语料+当前机制下的可达带；65%/PF3 需要「唤醒策略中存在
一个 spread_arb 级别的错配默认点」或「venue 流密度升级让微结构策略真正上线」——
两者都是可检验的命题，不是承诺。

---

## 6. 结论

1. **5 策略零信号全部结构性、全部数据面**：语料 spot=0 / book=0（356,749 事件实测），
   converter v2 输入面不含 spot/book 源；replay 消费端 8 类全支持。
2. **唤醒 = spot 收编（converter v3）一次解决触发面与 veto 面**，叠加各策略域内参数迁移
   适配 30–60s 的实际 top 节拍；唯一例外是 mad_dog 的亚秒 panic 前提，需内环改造或接受
   不可测。工作量排序：kline_probe < hot_side_momentum < flash_arb ≈ market_maker < mad_dog。
3. **「65%/PF3」的可度量性**：口径 A（单开）与口径 B（组合）在 v2 verdict 语料上都成立；
   WR/PF 只按 trade 结算现金流计算，cashflow（rebate/reward）单列不进分子；v1 口径数字
   （含单腿 PF 2.79）与 v2 不可比。5 策略唤醒前对 65%/PF3 是 0/0——唤醒是可测性的前提。
4. **差距分解**：WR 缺口 11.2pp（合法杠杆：域内寻优 +0~5pp、组合加权 +1~4pp）；
   PF 缺口 ×2.70（payoff 0.955→2.58 @53.8%WR，或 65%WR 下 payoff≥1.62；组合层是
   合法上探主路径，预期 ×1.2~1.8）。全杠杆用满的诚实可达带：组合 PF 1.5~2.5 /
   WR 55~63%；65%/PF3 的存在性已由 spread_arb 0.88 案例证明，但它依赖「错配默认点」
   或「数据密度升级」这两个可检验前提。
