# CAPACITY_AND_EQUITY — 容量、自我冲击与权益曲线口径（#192）

> 状态：容量门禁与权益/回撤门禁**已实现并在本文记录实测数字**；复利/加仓边界
> 已按内核真实旋钮对齐。容量曲线现在既能在 fixture 阶梯上跑，也能喂**真实盘口快照**
> （§2b：生产归档抽取 + 随代码提交的 fixture，买卖两侧都实测）。**未覆盖项在 §6
> 明确列出**（真实簿只有 2 个 token / 各一个瞬间、复利回放、真实 trade log 的 CI 接入等）。
>
> 本文只描述与门禁同源的数字：每一个数都能由 §1 的命令复现，或由内核源码的
> 具名常量给出。凡未实测的，一律写「未覆盖」，不写估计值。

## 1. 两条门禁（+ 一个只读的归档抽取工具）

```bash
# 容量与自我冲击曲线（默认 fixture 阶梯；--book 换成真实盘口快照，--side 选走哪一侧）
node scripts/capacity-check.mjs
node scripts/capacity-check.mjs --book /tmp/ladder.json --max-slippage-bps 50 \
     --max-order-notional 6 --max-shares 10 --json /tmp/capacity.json
node scripts/capacity-check.mjs --book docs/reports/data/capacity-gate/book-20260921T041048Z.json
node scripts/capacity-check.mjs --side sell --book docs/reports/data/capacity-gate/book-20260921T041848Z-mid.json

# 从生产归档抽一份真实盘口快照（流式、只读；--out 禁止落在 data/ 下）
node scripts/archive-book-extract.mjs --out /tmp/book.json          # 自动选档
node scripts/archive-book-extract.mjs --token <tokenId> --out /tmp/book.json
node scripts/archive-book-extract.mjs --self-test                   # 不需要归档
# 归档目录默认 `data/archive`（相对本 checkout）；真归档在生产 checkout，
# worktree 的 `data/` 只是门禁 scratch，所以跨 checkout 时显式写 --archive <绝对路径>

# 权益曲线/回撤门禁：真实日志
node scripts/equity-drawdown-check.mjs --trades data/trades/trades.jsonl \
     --initial-balance 6 --max-drawdown-pct 25 --min-trades 30 \
     --capacity /tmp/capacity.json --json /tmp/equity.json

# 权益门禁的数学与判决自检（CI 里跑的就是这条）
node scripts/equity-drawdown-check.mjs --self-test
```

三条脚本对生产状态都是**只读**的：读归档/trade log（和可选的容量报告），打印，退出。
容量门禁会拉起一个**临时 dry core**（私有 socket + scratch cwd，见
`scripts/lib/core-socket.mjs` / `child-guard.mjs`），不碰生产 socket 与 `data/`；
抽取脚本只读 `data/archive/*.jsonl`（流式，见 §2b），`--out` 落在 `data/` 下会直接拒绝（退出码 2）。
两条**门禁**都先做 `#172` 的来源校验，被测二进制必须是 git HEAD 的修订、`core.ready`
必须报同一个 commit（见 `scripts/lib/core-provenance.mjs`）；抽取脚本不碰内核二进制，
只读文件，因此没有这一层校验——它只负责把归档里的那一个事件原样归一化出来。

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

## 2b. 真实盘口实测（生产归档快照，2026-09-21）

§2 的阶梯是**示例账本**；本节是**生产归档**里真实盘口快照的实测。链路：

```text
<生产 checkout>/data/archive/events.jsonl  --(archive-book-extract.mjs：流式、只读、归一化)-->
      docs/reports/data/capacity-gate/*.json  --(capacity-check.mjs --book / --side sell)-->
      内核 dry 撮合的真实走单曲线
```

抽取脚本从归档**尾部**用 `readline` 逐行读（不整文件载入内存），读到
`--max-bytes`（默认 64 MB）或超出 `--max-age-min`（默认 120 min，按事件的 `at`）就停，
并把「读了多少字节/行」打印出来。它把每个 book 事件归一化成
`{"asks":[[price,size],…],"bids":[…]}`：**asks 升序、bids 降序**（都是最优档在前）、
同价合并、丢弃 size ≤ 0、字符串转数字。**asks 必须是升序**——`capacity-check.mjs --book`
按「最优档 = 第 0 档」走买侧阶梯，顺序错了会得到一条看起来合理但错误的曲线
（内核 `sim.rs::walk_marketable` 自己会排序，所以错误不会以异常形式暴露）。

先读取 `capacity-check.mjs` 确认：**本 PR 之前它只支持买侧**（`--book` 只读 `asks`，
限价固定 1.0）。本 PR 新增 **`--side sell`**：读 `bids`（要求**降序**、最优档在前）、
按卖侧限价走 bid 阶梯，滑点符号取反（`(best − vwap)/best`），账本断言改为
**记为「按走过的 VWAP 收款再扣费」**（`ledger.rs::settle_sell_fill`：
`balance + proceeds − fee`）。所以「假设退出侧深度对称」这句话不再需要：**两侧都实测**。

两条被 commit 的快照（`docs/reports/data/capacity-gate/`，各 ~3.7 KB，CI 可直接用）：

| 代号 | 文件 | token | 快照 UTC | 价差 | mid | 两侧档位数 / 总深度（首档） |
|---|---|---|---|---|---|---|
| **A** | `book-20260921T041048Z.json` | `9053526675…5063061`（77 位全量见文件 meta） | 2026-09-21T04:10:48.698Z | 0.0100 = **102.6 bps** | 0.975 | ask **11 档** / 296.88 股（首档 80）；bid **57 档** / 2253.09 股（首档 58.06） |
| **B** | `book-20260921T041848Z-mid.json` | `1018943702…2528113` | 2026-09-21T04:18:48.341Z | 0.0100 = **183.5 bps** | 0.545 | ask **32 档** / 4411.09 股（首档 72）；bid **35 档** / 4257.25 股（首档 60.06） |

**为什么是这两个**：A 是 `--token` 缺省时**自动选档**的输出；B 是显式 `--token`。
选档口径（写在脚本注释与 `--help` 里，不是魔法数）：

* 排的是**相对价差** `spreadBps = (best_ask − best_bid)/mid × 10000`，不是绝对价差。
  0.01 的 tick 在 (0,1) 二元市场上，绝对价差无法比较两本簿子（mid 0.975 的 0.01 是
  102.6 bps，mid 0.545 的 0.01 是 183.5 bps），只有相对 mid 才和门禁的
  `--max-slippage-bps` 同量纲。
* 「深度够」= **两侧最优档各 ≥ 50 股**（5× 现役 `min_shares = max_shares = 10` 的票面，
  即至少装得下 5 张现役单）**且两侧各 ≥ 5 档**（取 §2 fixture 阶梯自身的档位数：比它更粗的
  簿子量不出比 fixture 更细的曲线）。
* **这个口径有结构性偏差**：0.01 tick 的二元市场上相对价差随 mid → 1 单调变小，
  所以「最紧相对价差」几乎必然选中**接近结算**的 token（A 的 mid 0.975 正是这类）。
  mid 0.545 的代表（B）只能显式指定。这一条已写进 §6。

**这些数字测的是哪次提交**：下表的数字**首次产出**在二进制 `0.2.0+g655faf9260a4`
（git HEAD `655faf92`）上；脚本每次运行都打印 `bin … -> 0.2.0+g<sha>` 与
`want git HEAD=<sha>`，所以「测的是哪次提交」不靠记忆（提交后在同一批 fixture 上重跑，
曲线逐行一致，见下）。A 的抽取读到 67,108,846 字节 / 391,766 行 / 35,918 个 book 事件
（窗口内该快照比最新事件旧 712.6 s），B 读同一个窗口（扫描字节/行数相同）。

**快照是从哪个归档读的**：**生产 checkout** 的归档段
`/Volumes/Hard Disk/BlitzkriegBot/data/archive/events.jsonl`（读取时该段 187,644,731 字节，
扫描的是它最后的 67,108,846 字节）。这一点写在 fixture 的 meta 里：`archiveDir` 是解析后的
**绝对路径**，`command` 原样回显它。**不要把它读成 worktree 里的相对路径**：worktree 的
`data/` 是门禁自己的 scratch（`data/archive/events.jsonl` 是空文件），真归档只在生产
checkout；抽取脚本只对「本 checkout 自己的 `data/archive`」做相对化，其它路径一律照原样记录，
否则 fixture 会声称一个自己没读过的来源。

**归档里混着门禁自己的 scratch 簿**：实测该窗口有 **1 条** `cap-BTC` book 事件
（`at = 2026-09-21T02:59:19.450Z`）——`capacity-check.mjs` 用 `cap-<ASSET>` 发布它自己的
fixture 镜像簿，内核把它归档了进来。抽取脚本按 token 前缀 `cap-` 排除这类簿子
（fixture meta 里 `syntheticEvents: 1` 就是被排掉的那条），自检里有一条断言覆盖「一本
其它条件全过、只有 token 前缀是 `cap-` 的簿子必须被拒」。理由是本文的目标本身：测到它
答的是「示例阶梯有多深」，却会写成「某个 token 的容量」。该条本身也会被「两侧 + 未交叉」
检查拒掉，但前缀是**声明的规则**，不是那次运行的巧合。

**抽取重跑过**：首次抽取（提交前）与修正 provenance 后的重跑选中了**同一个 token、
同一个瞬间**，`asks`/`bids` 数组逐字节相同（只有 meta 多出 `archiveDir`/`syntheticEvents`
且 `command` 改为绝对路径）；再用本文所在 commit 构建的二进制
（`0.2.0+g<sha>`，sha 见 `git log -1 --format=%h -- docs/CAPACITY_AND_EQUITY.md`）
把四条曲线重跑，**逐行与下表一致**。

复现命令（本文所有数字都由这几条产生；`--max-slippage-bps` 取默认 100）：

```bash
# 抽取（归档在哪就写哪：真归档在生产 checkout；worktree 的 data/ 是 scratch）
node scripts/archive-book-extract.mjs --archive "/Volumes/Hard Disk/BlitzkriegBot/data/archive" \
     --max-bytes 67108864 \
     --out docs/reports/data/capacity-gate/book-20260921T041048Z.json          # A：自动选档
node scripts/archive-book-extract.mjs --archive "/Volumes/Hard Disk/BlitzkriegBot/data/archive" \
     --token 101894370274959081268501208843153007763679357303357413055804648102078227528113 \
     --out docs/reports/data/capacity-gate/book-20260921T041848Z-mid.json      # B：中价对照

# 买侧（走 ask 阶梯，限价 1.0）
node scripts/capacity-check.mjs --book docs/reports/data/capacity-gate/book-20260921T041048Z.json \
     --sizes 1,2,5,10,20,50,100,200,250,290,296
node scripts/capacity-check.mjs --book docs/reports/data/capacity-gate/book-20260921T041848Z-mid.json \
     --sizes 10,20,50,100,120,150,160,200,300,500,1000

# 卖侧（走 bid 阶梯，限价 = 阶梯最低价；本 PR 新增）
node scripts/capacity-check.mjs --side sell --book docs/reports/data/capacity-gate/book-20260921T041048Z.json \
     --sizes 1,5,10,50,100,120,130,150,250,500,1000
node scripts/capacity-check.mjs --side sell --book docs/reports/data/capacity-gate/book-20260921T041848Z-mid.json \
     --sizes 10,20,50,60,70,75,80,90,100,150,200
```

注意「可复现」的边界：**已被 commit 的快照 fixture 本身**是复现的输入（`--book <fixture>`
在两台机器上给出同一条曲线）；再跑一次**抽取**则会取「当下窗口里最新的那个 token」，
归档往前走了，选出来的 snapshot 就会不同——所以 provenance 记在 fixture 的 `meta` 里
（token / `at` / 扫描字节数与行数 / 来源段名），而不是靠重跑抽取脚本对齐。

卖侧的限价取**阶梯最低价**（A 是 0.001），不是「任意价 0.0」：风险层会拒
`price <= min_price`（`risk.rs`，默认 `min_price = 0`），0.0 在进撮合前就被 `RiskRejected`；
而 0.01 又会把走单停在 bid 还在 0.001 的簿子上（生产归档里就有这种簿子）。
另外风险层的名义额上限**只约束 BUY**（`risk.rs`：「SELL is bounded by position」），
所以卖侧曲线是**纯流动性**测量，caps 对比里的卖侧那一行是派生算术。

### 买侧（taker buy）

| 下单量 | **A**（best 0.98，mid 0.975） | **B**（best 0.55，mid 0.545） |
|---|---|---|
| 10 股（现役票面） | **0 bps**（vwap 0.98，9.80 USD） | **0 bps**（0.55，5.50 USD） |
| 20 股 | 0 bps（0.98，19.60） | 0 bps（0.55，11.00） |
| 50 股 | 0 bps（0.98，49.00） | 0 bps（0.55，27.50） |
| 100 股 | **6.7 bps**（0.98066，98.07） | **50.9 bps**（0.5528，55.28） |
| 120 股 | — | 72.7 bps（0.554，66.48） |
| 150 股 | — | 94.5 bps（0.5552，83.28） |
| 160 股 | — | 111.4 bps（0.556125，88.98） |
| 200 股 | **49.4 bps**（0.98484，196.97） | 170.9 bps（0.5594，111.88） |
| 250 股 | 61.4 bps（0.98601344，246.50） | — |
| 290 股 | 74.6 bps（0.98730717，286.32） | — |
| 296 股 | **77.0 bps**（0.98754419，292.31） | — |
| 300 / 500 / 1000 股 | 全簿只有 296.88 股 | 351.4 / 703.7 / 1046.4 bps |
| 超全簿 | 306.88 股 → **REJECTED** | 4421.09 股 → **REJECTED** |

**买侧容量（≤ 100 bps）**：A = **296 股 = 292.31 USD**；B = **150 股 = 83.28 USD**。

### 卖侧（taker sell，退出腿）

| 下单量 | **A**（best bid 0.97，mid 0.975） | **B**（best bid 0.54，mid 0.545） |
|---|---|---|
| 10 股（现役票面） | **0 bps**（vwap 0.97，9.70 USD） | **0 bps**（0.54，5.40 USD） |
| 50 股 | **0 bps**（0.97，48.50） | **0 bps**（0.54，27.00） |
| 60 股 | — | **0 bps**（0.54，32.40；首档 60.06） |
| 70 股 | — | 52.6 bps（0.53716，37.60） |
| 75 股 | — | 73.8 bps（0.536016，40.20） |
| 80 股 | — | **92.3 bps**（0.535015，42.80） |
| 90 股 | — | 123.2 bps（0.53334667，48.00） |
| 100 股 | **45.1 bps**（0.9656296，96.56） | 166.3 bps（0.531018，53.10） |
| 120 股 | **83.8 bps**（0.96187217，115.42） | — |
| 130 股 | 101.1 bps（0.96018969，124.82） | — |
| 150 股 | 128.9 bps（0.95749773，143.62） | 325.6 bps（0.522416，78.36） |
| 200 股 | — | 429.4 bps（0.516812，103.36） |
| 250 / 500 / 1000 股 | 217.1 / 346.1 / 475.9 bps | — |
| 超全簿 | 2263.09 股 → **REJECTED** | 4267.25 股 → **REJECTED** |

**卖侧容量（≤ 100 bps）**：A = **120 股 = 115.42 USD**；B = **80 股 = 42.80 USD**。

### 读法

* **断言在真实簿上也成立**（不是只对 fixture）：两条曲线都**单调**（更大的单从不付更少）；
  **最优档内零冲击**——断言口径是「量 ≤ 首档深度」（A 买 80 / A 卖 58.06 / B 买 72 / B 卖 60.06 股），
  这些口径内**实测过的**档位全是 0 bps；每一行成交都满足「账本 = 走过的 VWAP × 数量 ± 该价的费」
  （买侧扣、卖侧收）；超全簿的单是 **REJECTED 而不是部分成交**。真实簿没有让任何一条断言变红。
* **容量由最优档决定，不是总深度**：fixture 阶梯的首档恰好 10 股（= 现役票面），
  所以 fixture 容量就是 10 股；真实簿首档 58–80 股，容量立刻上一个量级。
* **同一 100 bps 预算下，真实簿容量比 fixture 阶梯更大**：A 买 296 股（29.6×）/ 292.31 USD（73.1×）、
  A 卖 120 股（12×）/ 115.42 USD（28.9×）、B 买 150 股（15×）/ 83.28 USD（20.8×）、
  B 卖 80 股（8×）/ 42.80 USD（10.7×）。**同尺寸 100 股**对比更直观：fixture 买侧 500 bps，
  A 买 6.7 / 卖 45.1，B 买 50.9 / 卖 166.3 bps。
* **退出侧不是对称的，而且更贵**：同一 token、同一瞬间，100 股时 A 买 6.7 bps vs 卖 45.1 bps（6.7×）、
  B 买 50.9 vs 卖 166.3（3.3×）。旧 §6「假设退出侧深度对称」的写法因此作废。
* **现役票面（10 股）在四条真实曲线上都是 0 bps**：在这个尺寸上咬人的是费（§3）与
  token 的赔付结构，不是深度。
* **A 的买侧几乎「整簿可吃」**：全部 asks 只有 296.88 股 / 293.19 USD，连吃光整簿也只有 77 bps；
  而 A 的卖侧虽然挂着 2253.09 股，**头 120 股就吃掉 100 bps**——「深度大」和「容量大」不是一回事。
* **中价簿（B）才是容量紧张的一侧**：B 的总深度（4411 股）比 A（297 股）大一个量级，
  但 100 bps 内的容量反而更小（买 150 vs 296 股），因为 B 的最优档只有 72 股且价位密集在
  0.55–0.60 之间。**容量是局部量，不是簿子的总量。**

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
得到该笔的自我冲击；超出阶梯的尺寸截断到最后一个测点并计数。曲线可以是 fixture 阶梯，
也可以是 §2b 的真实盘口（报告里的 `side` 说明它测的是哪一侧，`bestPrice` 是该侧最优价；
旧报告里这个字段叫 `bestAsk`，两种都读）。它是「一笔一条腿、只算报告测过的那一侧」的
下界，只用于回答「这笔净额里有多少可能是深度成本」，不用于预测。

## 6. 未覆盖（如实列出）

1. **真实盘口快照已接入，但只有 2 个 token、各一个瞬间**：容量曲线现在能喂真实快照
   （`--book docs/reports/data/capacity-gate/*.json`，快照随代码提交、CI 可直接复现，见 §2b），
   抽取是**流式只读**的（`scripts/archive-book-extract.mjs`），买卖两侧都有实测曲线——
   **「没有真实快照」这句已不成立**。仍属未覆盖的部分：
   * 只有 **2 个 token**（A 自动选档、B 中价对照）、**各一个时间点**：没有跨时间的深度分布，
     也没有「同一 token 在不同时刻的容量」；
   * 自动选档口径（最紧**相对**价差）**结构性偏向接近结算的 token**（0.01 tick 下相对价差
     随 mid → 1 而变小），A 的 mid 0.975 就是这一类；中价代表只能显式 `--token`；
   * 抽取窗口是归档尾部（`--max-bytes 64 MB` / `--max-age-min 120`；本次实际读到
     391,766 行 / 35,918 个 book 事件就停），不是全量归档，也不是连续跟踪；归档里
     还混着**门禁自己**的 scratch 簿（本次 1 条 `cap-BTC`，按前缀排除，见 §2b）。
   即：**这是「真实簿的实测」，不是「真实簿的分布」**。
2. **没有复利回放**：本文给出的是旋钮之间的关系（§4）与前端容量（§2/§2b），
   没有「按权益百分比增长下单量」的逐笔重放，因此**没有复利下的权益曲线证据**。
3. **两侧冲击都已实测，但仍是单 token、单瞬间、dry 撮合模型**：本 PR 新增 `--side sell`
   （走 bid 阶梯，逐档 VWAP 就是退出腿的自我冲击），§2b 给出 A/B 两个 token 的
   **买、卖两条真实曲线**；旧文「假设退出侧深度对称（实际只测了买侧）」作废——
   实测是**退出侧更贵**（100 股：A 买 6.7 vs 卖 45.1 bps；B 买 50.9 vs 卖 166.3 bps）。
   仍属近似的地方：
   * 一笔一条腿，不是**往返重放**；滑点在实测曲线上按股数**插值**；
   * 单瞬间的簿子：没有「挂单/成交期间簿子变化」；
   * 仍是 dry 走单模型（不排队、不部分成交、按档位线性吃满）；
   * 卖侧限价取阶梯最低价（0.0 会被风险层 `min_price` 拒，§2b 有说明），且风险层的
     名义额上限只约束 BUY——卖侧曲线是**纯流动性**测量。
4. **Sharpe 的年化口径**：`--trades-per-year` 或中位间隔二者选一，样本 < 30 笔不强制；
   不同 cadence 下的 Sharpe 不可直接比较（脚本会打印所用 cadence）。
5. **CI 只跑 `--self-test`；真实盘口输入已随代码提交，但默认路径仍不依赖它**：
   * 权益门的**真实 trade log** 仍不在仓库里，真实日志模式属于运维动作（部署机上手动/定时跑）。
     因此 CI 证明的是「判决会响」，不是「当前账户曲线健康」——这一条未变；
   * 容量的**真实盘口**已经在仓库里：`docs/reports/data/capacity-gate/*.json`（2 个 token，
     各 ~3.7 KB，带 token / 快照 UTC / `archiveDir`（读的是哪一份归档）/ `command` /
     来源段名 / 扫描字节数与行数 / 被排掉的 scratch 簿计数等 provenance），
     CI 可以直接跑买侧 `--book …` 与卖侧 `--side sell --book …`；
   * 但**默认的 `node scripts/capacity-check.mjs`（fixture 阶梯）没有被替换**：它是稳定的 CI 基线
     （不依赖归档、不依赖网络），真实簿是**追加**输入，不是替代；
   * `node scripts/archive-book-extract.mjs --self-test`（21 条断言：归一化顺序、同价合并、
     零/负 size、(0,1) 之外的价、单边/交叉簿、**`cap-` scratch 簿**、相对价差排序、
     两侧深度与档位地板、并列时取更深/更新的簿）**不需要归档**，CI 里可当纯自检跑；
   * 仍未覆盖：**CI 里喂真实 trade log**、以及「容量随时间的漂移告警」。
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

# 真实盘口（§2b）：抽取 + 两侧跑一遍，不需要网络
node scripts/archive-book-extract.mjs --self-test
node scripts/archive-book-extract.mjs --archive "/Volumes/Hard Disk/BlitzkriegBot/data/archive" \
     --max-bytes 67108864 --out docs/reports/data/capacity-gate/book-20260921T041048Z.json
node scripts/capacity-check.mjs --book docs/reports/data/capacity-gate/book-20260921T041048Z.json \
     --sizes 1,2,5,10,20,50,100,200,250,290,296
node scripts/capacity-check.mjs --side sell --book docs/reports/data/capacity-gate/book-20260921T041048Z.json \
     --sizes 1,5,10,50,100,120,130,150,250,500,1000
```

三者均先打印被测二进制的 `--version`（`0.2.0+g<sha>`）与 `core.ready` 的 commit，
所以「这份数字测的是哪次提交」不靠记忆。
