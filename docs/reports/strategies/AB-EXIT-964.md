# stone A/B 出场对照实验报告（issue #964）—— 模板 + 框架交付

> **状态：框架已交付，真数据待跑。** 本文先固化实验结构与预声明口径（数字
> 存在之前钉死，#964 验收 3 的「不挑窗口」），真数据由
> `scripts/ab-exit-964.mjs` 跑出后回填（`待补` 标记处）。**待 #962 合并后
> 跑真数据**；A 臂语料坐标的裁决项见「待跑清单」第 3 条。

- 分支：`feat/issue-964-ab-exit-lab`
- 脚本：`scripts/ab-exit-964.mjs`（一键复跑；断点续跑；`--dry-run` 出计划；
  `--self-test` 离线框架自测）
- 回放引擎：`blitzkrieg-core --backtest`（stone 内核回放，无替代方案）
- B 臂策略包：`coin_flip_exit_lab`（#962 交付物，本 issue 只调用不修改）

## 1. 预声明坐标（跑之前钉死）

| 项 | 声明 |
|---|---|
| 语料主坐标 | `data/onchain/` 15m-act 单文件：4,766 轮（roundSlot），2026-08-05 → 2026-09-30，资产 BNB/BTC/DOGE/ETH/SOL/XRP，356,749 事件 |
| 语料门禁 | manifest `sha256.events` 对文件实哈希；`scripts/corpus/fix_resolutions.py --check`（#890：label-form settlement 行 = 拒绝回放）；正式结算标签走 `relabel_official.py --apply-corpus`（需网络，操作员一次性前置） |
| 窗口 | 轮数等分连续切片，默认每窗 ≥200 轮（`--rounds-per-window 200` → 23 窗）；每窗强制校验「本窗各轮的 resolution 行都在切片内」（resolution 与下一轮 round 行同毫秒交错时尾边界回拉；不完整 = 硬错，绝不静默丢结算） |
| 写实档（主） | `--fee-model official --slippage-ticks 1 --backtest-tick-ms 50 --backtest-fast` |
| identity 档（附录上界） | 引擎默认恒等成交模型（`--profile both` 顺带跑，只进附录） |
| 资金 | `--seed-balance 1000`；`--min-shares 1 --max-shares 1`（top-only 语料的簿深度是名义 1 手，`marketdata.rs update_top`；3 手单永远吃不满 —— 语料坐标决定的 sizing，两臂同值）；`--max-order-notional 12`；`--max-positions 10` |
| seed | B/C 臂 `coin_seed=962`（`--backtest-knob` 显式下发并记录）；A 臂无 seed（确定性引擎） |
| 确定性 | 每臂 × 每窗双跑，报告文件 sha256 逐字节一致 + `tradesSha256`（#351）相同；不一致 = 确定性 bug，实验判无效而非统计噪音 |
| 环境隔离 | 每臂只 stage 目标包到临时目录（#302 兄弟包耦合：默认 lua 目录会加载全部兄弟包）；`--no-strategy-dir`；报告 `strategies[]` 出现别的名字 = 归因污染，实验判无效 |
| 窗口纪律 | 全部窗口进附录（验收 3）；报告的每个数字可追溯到 run 目录产物（验收 4） |

### 1.1 平均持仓时长 —— 预先声明的近似口径

backtest 报告的 `tradeLines` 不带逐笔时间戳（引擎 schema 限制，`backtest.rs`
TradeLine）。逐笔秒数在本 issue 内不可测（改引擎属禁止触碰项）。本实验用
**round_slot 近似**并预先声明：

- 每笔交易按 `conditionId` 映射回语料轮次；持仓只可能存在于该轮窗
  `[expiresAt − roundDurationSec, expiresAt]` 内（15m 语料 = 900s）。
- 报三个量：
  - `settlementRatePct`（精确）：出场 reason 分桶的到期结算占比；
  - `avgHoldSecFloor`（下界口径）：结算笔记满轮、提前出场笔记 0；
  - `avgHoldSecUpper`（上界口径）：全部笔记满轮（15m 恒 900s）。
- 真实平均持仓 ∈ [floor, upper]。跨臂比较持仓结构以 `settlementRatePct`
  与 reason 直方图为准（精确可测）。逐笔时长需引擎给 tradeLines 加时间戳，
  另开 issue。

## 2. 臂定义（#964 × #962 约定）

| 臂 | 包 | 入场 | 出场（被测对象） | seed |
|---|---|---|---|---|
| A | `lua_momentum`（仓库内 0.5.0） | 动量信号，纯 maker（manifest `entry_maker: true`，宿主强制 never-escalate） | **原默认出场**：manifest 出厂 tunables，一个旋钮不动 | — |
| B | `coin_flip_exit_lab`（#962） | 抛硬币（coin_seed × round_slot × condition_id 的确定性哈希），taker 风格（`entry_maker: false`，回放 `--entry-maker-timeout-ms 1`） | **动态出场栈默认**：硬止损 + 时间加权止盈带 + 移动止盈 + 截止线 + thesis_death | 962 |
| C（对照） | `coin_flip_exit_lab`（同包同 seed） | 同 B 同 seed | **出场栈 `--backtest-knob` 全清零**：hard 0 / tp 0(+tight) / trail 0(+tight) / death reversal=100, tight=0 / stale=0，只剩 `exit_time_left_sec` | 962 |

- A vs B 回答 issue 原题（好入场+固定出场 vs 随机入场+动态出场）；
  B vs C 把出场机制的孤立贡献（= B − C）从入场里剥出来；C 与 B 同 seed
  同窗口同资金，差异只能来自出场栈。
- C 臂清零口径逐字对应 #962 包 README 的对照组声明。如实声明：stdlib
  thesis_death 的「盘口消失仍判死」不受 `stale=0` 影响（那是超时阈值，
  不是存在性判死开关）—— 该 runtime 注入路径两随机臂同享。
- 两臂环境差：只有包（与包声明的入场机制）。语料/窗口/资金/sizing/费率/
  滑点/虚拟钟/holder 兜底阶梯完全一致（脚本自测含逐旗标一致性断言）。

## 3. 指标口径（全部从报告产物直出，不手算）

| 指标 | 来源 |
|---|---|
| 胜率 | `trades.winRatePct` |
| 盈亏比 | (毛利/胜数)/(毛损/负数) |
| 期望（USD/笔） | `trades.netPnlUsd / closed` |
| 最大回撤 | `trades.maxDrawdownUsd`（正数幅值，越低越好） |
| 单笔最大亏损 | tradeLines `netPnlUsd` 最小值 |
| 尾亏 P95/P99 | tradeLines 亏损额分布（netPnlUsd<0 取绝对值），线性插值分位；逐窗 tradeLines 导出 `trades/&lt;profile&gt;/<臂>/&lt;win&gt;.trades.jsonl` |
| 平均持仓时长 | §1.1 声明的 round_slot 近似（floor/upper + settlementRatePct） |
| 出场结构 | reason 直方图（Settlement/TimeExit/TakeProfit/TrailingStop/StrategySignal(含 thesis_death)/…） |
| 入场饥饿证据 | 每窗 `ordersPlaced/ordersRejected/rejectionCauses/accountFreezes` |
| 确定性 | 双跑报告 sha256 + `tradesSha256` |

## 4. 对照表与结论（模板 —— 真数据后补）

> 脚本按本节结构自动生成 `ab-exit-964-&lt;profile&gt;.md`；本节先放结构，
> `待补` 处由真数据回填。

### 4.1 门禁

| 门禁 | 结果 |
|---|---|
| 覆盖（每窗 ≥200 轮，共 ≥200 轮/臂） | 待补 |
| 确定性（每臂×每窗双跑逐字节一致） | 待补 |
| 归因污染（报告只含本臂策略名） | 待补 |

### 4.2 B vs A（issue 原题）

| 指标 | A | B | Δ(B−A) | 判定 |
|---|---|---|---|---|
| 胜率 % | 待补 | 待补 | 待补 | 待补 |
| 盈亏比 | 待补 | 待补 | 待补 | 待补 |
| 期望 USD/笔 | 待补 | 待补 | 待补 | 待补 |
| 最大回撤 USD | 待补 | 待补 | 待补 | 待补 |
| 单笔最大亏损 USD | 待补 | 待补 | 待补 | 待补 |
| 尾亏 P95 / P99 USD | 待补 | 待补 | 待补 | 待补 |
| 结算率 % / 持仓 [floor, upper] s | 待补 | 待补 | 待补 | info |

### 4.3 B vs C（出场栈的孤立贡献，同 seed 同窗口同资金）

| 指标 | B（动态出场） | C（清零对照） | Δ(B−C) |
|---|---|---|---|
| 期望 USD/笔 | 待补 | 待补 | 待补 |
| 盈亏比 | 待补 | 待补 | 待补 |
| 尾亏 P99 | 待补 | 待补 | 待补 |

### 4.4 结论模板（判定由脚本给出，禁止人工改写）

- `B-better-on-both-target-metrics`：B 在期望与盈亏比同时优于 A → 出场机制
  的质变作用在本坐标成立；孤立贡献 = B−C 期望差。
- `B-better-on-enough-metrics`：B 在 ≥2 项指标占优但目标两项未同时翻转 →
  先看 B−C，再走归因清单。
- `B-not-better`：按归因清单走，禁止直接改成「出场无用」。
- `arm-starved`：参比臂全程 0 平仓 → 对照不成立，判结构性归因（下条是
  本框架在自证运行中已实测到的真实结构，真数据跑批将复用同一判据）。
- `invalid`：门禁未过，报告不可作为证据。

### 4.5 归因清单（B 不优时按序排查）

1. **出场参数**：B 的栈是 #962 网格默认点，非调参最优。若 B−C ≤ 0，跑
   #962 的 16 点预声明网格（`scripts/coin-flip-lab-replay.mjs`）。
2. **样本**：附录逐窗表 —— B 优于 A 的窗口若集中在少数几窗，扩窗重跑。
3. **市场结构**：写实档与 identity 档结论翻转 → 摩擦成本主导；资产结构
   （6 资产混合）单资产拖尾 → 分资产重算。

## 5. 框架自证（已跑 —— 这是框架验证，不是实验结论）

以下为本 PR 交付时实际执行的验证记录（证据在 PR 描述与 run 目录）：

- `node --check scripts/ab-exit-964.mjs`：通过。
- `node scripts/ab-exit-964.mjs --self-test`：9 项断言全过（分位口径 /
  指标抽取 / 跨窗合并 / 判决方向 / 对照旋钮口径 / 命令装配一致性）。
- `--dry-run`：A 臂计划 23 窗 × 2 跑 = 46 次回放正常输出；**缺 B 包时
  硬错退出 2**（显式报错，非静默降级）—— 本分支无 `coin_flip_exit_lab`
  包，全臂 dry-run 以 exit 2 拒绝，输出指明 #962 未合并。
- 3 臂端到端（自证 run，`--windows 3`，`--b-pkg-dir` 指向 #962 工作树包，
  仅验框架不作结论）：双跑确定性 3 臂全 match；A 臂实测饿死（详见 §6）；
  B 臂 5,422 平仓 / C 臂 3,903 平仓，指标、尾亏、对照表、附录、`--report-only`
  重渲染全部产出。

## 6. 已知结构性发现（预登记，真数据跑批的裁决输入）

**主坐标语料对 A 臂是锁价簿。** `data/onchain` 15m-act 的 top 报价
100% 锁价（ba==bb，实测 293,286/293,286 —— 单边成交价派生）。A 臂
（lua_momentum）入场由 manifest `entry_maker: true` 声明为纯 maker
（宿主强制 never-escalate，`lua_loader.rs`），提交瞬间 buy@best_bid ==
best_ask → `venue.WouldCross` 拒单 → 5 连拒触发内核账户冻结 → 整窗无
入场。这是包学说（「错失机会，从非损失」）× 语料结构的交互，不是旋钮
可修的（改策略实现/引擎均属 #964 禁止触碰项）。

脚本对这种情况的处置：如实报 `arm-starved`（附拒单/冻结证据），对照
判定全部 n/a，绝不把「对手没上场」读成「B 更优/不优」。真数据跑批需
审计裁决坐标，见待跑清单第 3 条。

## 7. 可追溯性

- run 目录（默认 `data/evolution/ab-exit-964/&lt;UTCstamp&gt;/`）：
  - `ab-exit-964.json` —— run manifest（语料 sha、窗口切片区间与 sha、
    臂声明、包 sha256、全部指标、判决）；
  - `ab-exit-964-&lt;profile&gt;.md` —— 本模板的实数据渲染；
  - `windows/win-*.jsonl` —— 窗口切片（复跑输入，`slices-manifest.json`
    记录行区间 + sha256）；
  - `runs/&lt;profile&gt;/<臂>/&lt;win&gt;/report-{1,2}.json` —— 双跑原始报告
    （旁置 `.cmd.txt` 防陈旧缓存）；
  - `trades/&lt;profile&gt;/<臂>/&lt;win&gt;.trades.jsonl` —— tradeLines 导出
    （尾亏分布的原始证据）。
- 重渲染：`node scripts/ab-exit-964.mjs --report-only &lt;runDir&gt;`。

## 8. 待跑清单（真数据前置条件）

1. **#962 合并**：`coin_flip_exit_lab` 进 `user_layer/strategies_lua/` 后，
   无需 `--b-pkg-dir` 直接跑（缺失时脚本 exit 2 拒绝，不降级）。
2. **语料前置**：`python3 scripts/corpus/fix_resolutions.py --check &lt;corpus&gt;`
   （脚本已内联为门禁）；如需官方结算标签重钉：
   `relabel_official.py --apply-corpus`（需网络，操作员执行，脚本校验
   manifest sha256 防漂移）。
3. **A 臂坐标裁决（审计输入，§6）**：主坐标锁价簿饿死纯 maker 入场 ——
   (a) 接受主坐标下的 `arm-starved` 结构性结论；或 (b) A 臂换档案书语料
   （data/archive frozen-corpus 惯例，真 L2 档差，#962 同款）重跑 A/B。
   裁决后命令不变，`--corpus` 指向裁决语料即可。
4. **真数据跑批**：
   `node scripts/ab-exit-964.mjs`（默认 23 窗 × A/B/C × 双跑 = 138 次回放，
   断点续跑）；identity 档附录加 `--profile both`。
5. **回填本报告** §4 对照表与结论，PR 正文更新「待跑」为实测。

## 9. 禁止触碰核对

- 未修改任何策略实现（A/B/C 全部调用既有包；B/C 为 #962 产物）。
- 未修改回测引擎核心（backtest.rs 只读；无引擎侧新字段）。
- 无未来函数/事后选样：窗口在跑之前由轮数等分预声明，全部窗口进附录，
  判定规则（≥2 指标 + 目标两项）在脚本内固定。
- Mimosa 凭据：本 PR 无凭据、无语料入库（语料 stay in data/，gitignored）。
