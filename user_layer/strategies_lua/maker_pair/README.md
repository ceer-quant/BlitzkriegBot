# maker_pair

The both-restable maker pair — issue #871's answer to
`pair_discount_arb`'s structural loss mode. A binary-round pair is quoted
ONLY when every leg can rest as a post-only buy strictly under its own ask,
and a single leg is never chased, topped up, or repaired. Leg dead = leg
dead.

## Mechanism

When the sum of the two RESTABLE maker quotes is under the cap,

```
quote_UP + quote_DOWN < max_pair_cost        (default 0.985, strict)
```

the package emits BOTH legs in one evaluation cycle — EQUAL declared share
counts (default **1 share per leg**), each priced where a post-only buy can
actually **rest**. Otherwise it emits NOTHING. There is no third outcome.

## How it trades

- **Both or nothing** — every leg must carry a book the kernel would accept
  a post-only POST into: fresh, a real ask, visible ask depth, and a quote
  strictly UNDER that ask (the kernel refuses an at-ask post-only buy at
  POST — `#464`, `venue.WouldCross`). ONE leg failing any guard abandons
  the WHOLE attempt: zero entries, not one. An abandoned attempt is not an
  attempt — nothing is armed, and a later cycle with both books back
  re-tests the pair from scratch.
- **The cost test runs on the quotes** — a maker fill prints at its quote
  (at or below it, never worse), and a post-only maker leg pays no fee, so
  the honest all-in cost of the pair is `quote_UP + quote_DOWN`.
  `bk.fees()` is never read: no fee schedule in force is NOT an error here,
  because no fee enters this package's cost model — the manifest declares
  `entry_maker`, so the kernel routes every entry post-only and the taker
  schedule never applies. A venue that charges maker fees must be priced
  into `max_pair_cost` by the operator.
- **No single-leg repair** — one attempt per condition per round
  (`armed[condition] = round slot` on emission). A leg that is rejected at
  POST, unfilled, or partially filled stays exactly as it is: no top-up, no
  re-quote, no taker escalation, no merge intent. The naked remainder rides
  to settlement (`holds_to_settlement`), which is the accepted structural
  loss — and the reason the default size is one share per leg. The kernel's
  naked-leg requote exemption (issues #904/#848) keys on hold-to-settlement
  strategies that RE-EMIT leg intents while holding a single side; this
  package never emits one, so the exemption never flows for it.
- **No collection either** — unlike `pair_discount_arb`, this package emits
  no `merge` intents. A complete pair redeems exactly $1.00 at settlement
  regardless; merging early would only recycle collateral this package
  never reuses (one attempt per condition per round).
- **Quote modes** — `entry_quote_mode` picks the depth of the restable
  quote: 1 = ask − `entry_reprice_ticks` (default — the most aggressive
  RESTABLE quote: the falling ask has to step only one tick onto us), 2 =
  mid floored to the 0.01 grid, 3 = the bid. There is NO at-ask mode: the
  at-ask price is the one price a post-only buy can never rest at (#464),
  and this package's contract is "strictly under the ask".

## Kernel surfaces used (zero kernel changes)

| declaration | effect | source |
|---|---|---|
| `entry_maker: true` | every entry submitted post-only, never escalates to taker | #742 |
| `atomic_batch: true` | the two legs of one cycle are ONE economic ticket for an E-VA pot | #531 |
| `holds_to_settlement: true` | pair-completion entry exemption; exit ladder leaves the legs alone in dry/read-only | existing |
| `trailing_exit` ABSENT | follows the kernel-wide value (the three-state default) | #742 |

`atomic_batch` arms nothing by itself: the pot comes from
`CoreConfig::virtual_limits`, and **no pot ships for this package — the
allowlist stays empty** until the freeze-window replay and soak gates both
say GO.

## 红线 posture

- The package registers DISABLED (the startup scan registers every package
  disabled); enablement is the operator's account binding, nothing else.
- `DRY_RUN` default untouched; no start script; no live enablement.
- Default 1 share per leg: at mid prices a leg is ≈ $0.50, a pair ≈ $1, and
  one order sits inside the 100 USDC config's $2 single-order notional.

## Tunables

| name | default | meaning |
|---|---|---|
| `max_pair_cost` | `0.985` | all-in pair ceiling, tested on the sum of the two quotes (strict `<`) |
| `shares_per_leg` | `1` | per-leg size in shares, identical on both legs by construction |
| `min_time_left_sec` | `30` | entry time floor (a maker quote needs round life left to be walked into) |
| `entry_quote_mode` | `1` | 1 = ask − ticks, 2 = mid floored to the grid, 3 = the bid (no at-ask mode) |
| `entry_reprice_ticks` | `1` | ticks under the ask for mode 1 |

## 回放报告（2026-11-03，优化轮 B 实测）

**本包已在冻结语料上跑了六臂回放网格：`entry_quote_mode ∈ {1,2,3}` × `max_pair_cost ∈ {0.985, 0.99}`。全部六臂非负，`mode 3`（bid 报价）显著最优且每对净利恒正；两档 `max_pair_cost` 在本语料上逐位不可区分；`mode 1` 实证了裸腿风险。** 证据等级：**synthetic-replay-not-real-evidence** —— 回放是合成执行（真实 L2 语料 + 模拟撮合），以下数字不得作为真实盘证据引用。

### 结论先行

1. **`entry_quote_mode=3`（bid）是唯一持续触发的臂**：4 资产窗口共 17 次尝试，17 对全部双腿成交（入场率 100%），每对净利 = 1 − pair_cost ∈ [+0.024, +0.067] USD，合计净 +0.6005 USD（PF 1.0675；含 modeled maker rebate +0.1105 后 +0.7111）。pair_cost 实测分布 0.94–0.98，全部严格低于 0.985。
2. **`mode 1`（ask − 1 tick）触发稀疏且裸腿风险实证**：3 次尝试中 2 次单腿成交（BTC/XRP：down 0.01 被 trade-through 穿价成交、对手腿挂单至窗口结束无人穿价）——正是本包「腿死=腿死」设计所接受的形状。回放窗口内裸腿未成交，净 +0.0008 USD（PF 1.0278）；真实世界里那条挂死的腿会在结算前给出 $0 或 $1 的敞口，回放无法为其定值。
3. **`mode 2`（mid 地板）触发最少**（3 次尝试全成交，净 +0.0791）：mid 和被 0.01 网格地板后几乎总是 ≥ 0.99，被成本闸挡住——该模式在本语料的二元轮盘 book 上天然近乎沉默。
4. **两档 `max_pair_cost` 逐位零差异**：六臂的 `tradesSha256` 在 0.985 与 0.99 之间完全相同。原因：所有实际触发的报价和 ≤ 0.98，没有任何尝试落在 [0.985, 0.99) 的边缘带——本语料无法区分两档，**收紧到 0.985 也没有在本窗口付出任何被拒尝试的代价**。

### 语料与口径

- **语料**：#908 验收的 4 份 polymarket 家族冻结切片（a4-livereplay 2026-10-27 实盘镜像归档 → `scripts/corpus/filter_family.py --venue polymarket` 单家族切片 → `scripts/corpus/relabel_official.py --apply-corpus` 官方胜负改写 8/8 全中 → `fix_resolutions.py --check` 门通过）。每资产一段：`corpus-{BTC,ETH,SOL,XRP}-pm.jsonl`，各 4,754 行。
- **窗口**：`1793109600000 → 1793116805616`（7,205,616 ms 虚拟时钟 ≈ 2.0 小时，每资产 8 个 900 s 轮）。
- **语料 sha256**（与 #908 产物清单逐位一致）：

| 文件 | sha256 |
|---|---|
| corpus-BTC-pm.jsonl | `1dea4d5f58db29e77f92e55a1574bfdee0aba970e6df672edec172d707a8d3c7` |
| corpus-ETH-pm.jsonl | `299d924bdc5d0df11b47aeb1f0c968a081f0bfc644772df9aaa972ca8338ce1f` |
| corpus-SOL-pm.jsonl | `d69405256fe752d007c960b58995708c4d8d316a051d2fd5cb0757282f16470b` |
| corpus-XRP-pm.jsonl | `e11f9d4119440cc3ca0e0e9d7f99ddf532be18a6f042ed7ddba7d86d19bd431d` |

- **回放口径**：`--fill-model l2-delay`（#908 预设：taker 决策→venue 150 ms + 报告 RTT 150 ms、镜面 L2 逐档 FOK、**maker trade-through**——挂单只在市场严格穿价时成交，touch 不算）+ `--fee-model official`（Polymarket 官方 taker 费表 rate=0.07）+ `--maker-rebate`（#741 模型：maker 成交计 `0.20 × taker-fee-equivalent`）。
- **命令**（逐字，每资产 × 每臂各跑一次，`$CORPUS`/`$MODE`/`$CAP` 为网格变量）：

```bash
blitzkrieg-core --backtest "$CORPUS" \
  --engine --enable-strategy maker_pair --lua-strategy-dir <staged 单包目录> \
  --round-sec 900 --min-round-age 30 --min-time-left 180 \
  --max-order-notional 6.00 --seed-balance 100 \
  --backtest-tick-ms 50 --backtest-tail-ms 100 \
  --fee-model official --no-strategy-state --no-intent-audit \
  --fill-model l2-delay --maker-rebate \
  --backtest-knob "maker_pair:entry_quote_mode=$MODE" \
  --backtest-knob "maker_pair:max_pair_cost=$CAP" \
  --backtest-report out.json
```

- `strategy.lua` / `manifest.json` 零改动：两参数全部经 `--backtest-knob`（counterfactual hot-params 通道）注入，包默认值未被触碰；回放读的是 worktree checkout 的 `user_layer/configs/execution_policy.toml`（base b30b205a）。

### 六臂横向对比（4 资产窗口求和；0.985 档，0.99 档逐位相同）

| 臂（mode / cap） | 尝试 | 双腿齐成交 | 裸腿 | 腿成交/发出 | 平仓笔 | 毛 + | 毛 − | taker 费 | maker rebate | 净利 | 净利+rebate | PF |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 / 0.985 | 3 | 1 | 2 | 4/6 | 4 | 0.0304 | 0.0296 | 0.00 | 0.0008 | **+0.0008** | +0.0016 | 1.0278 |
| 1 / 0.99 | 3 | 1 | 2 | 4/6 | 4 | 0.0304 | 0.0296 | 0.00 | 0.0008 | **+0.0008** | +0.0016 | 1.0278 |
| 2 / 0.985 | 3 | 3 | 0 | 6/6 | 6 | 1.5995 | 1.5204 | 0.00 | 0.0191 | **+0.0791** | +0.0982 | 1.0520 |
| 2 / 0.99 | 3 | 3 | 0 | 6/6 | 6 | 1.5995 | 1.5204 | 0.00 | 0.0191 | **+0.0791** | +0.0982 | 1.0520 |
| 3 / 0.985 | 17 | 17 | 0 | 34/34 | 34 | 9.4950 | 8.8944 | 0.00 | 0.1105 | **+0.6005** | +0.7111 | 1.0675 |
| 3 / 0.99 | 17 | 17 | 0 | 34/34 | 34 | 9.4950 | 8.8944 | 0.00 | 0.1105 | **+0.6005** | +0.7111 | 1.0675 |

口径注记：

- **角色分布**：六臂全部成交均为 post-only **maker** 入场（0 taker 成交、0 post-only 拒单、taker 费全臂 0.00）——`entry_maker` 声明如设计生效。maker rebate 单列不计入「净利」列（回放把它记为 venue 现金流，`trades.net` 不含它）。
- **胜率语义**：完整 pair 必然一赢腿（$1 结算）一输腿（$0 结算），W/L ≈ 1:1 是机械结构不是信号质量。每对净利 = `1.00 − (quote_UP + quote_DOWN)`，m3 实测 pair_cost 0.94–0.98 → 每对 +0.024～+0.067。
- **入场率**：尝试 = 双腿条件满足的一次发出（每次 2 单）；m3 的 17/17 全成交即 100% pair 完成率——trade-through 口径下 bid 挂单被穿价是常态，这正是挂单的逆向选择通道，pair 的对称结构（两腿同挂同成本闸）使成交即锁定 `1 − pair_cost` 的正边际；真正的尾部风险是裸腿（mode 1 实证）。

### 诚实边界（局限）

- **单窗口、小样本**：每资产 8 轮、约 2 小时、合计 23 次尝试 / 21 个完整对。不做任何统计显著性主张。
- **合成执行**：l2-delay 是模型不是 venue；结算前 venue 侧撤单/结算机制细节未建模；裸腿在回放里骑出窗口（`liveAtEnd`），其真实结算值无法定值——mode 1 的 +0.0008 对裸腿风险是**低估也是高估**，方向不可知。
- maker rebate 是 `0.20 × 自身名义额的 taker 费等效` 的池份额近似（回放看不到真实 taker pool），量级 $0.0001–0.03/臂，不影响符号。

### 建议（不在本 PR 改 manifest —— 默认值变更是运营者裁决）

1. 若运营者考虑启用，本语料上**证据最厚的是 `entry_quote_mode=3`（bid）**；默认值 `1` 的触发条件在本语料的边缘 book 上近乎不满足，且三次触发两次裸腿。
2. `max_pair_cost` 0.985 → 0.99 在本语料**不可区分**（无边缘尝试被任何一档改变命运）；放宽它没有观测到收益，维持默认即可。
3. 下一步证据应是**更长窗口 + 更宽语料的多段冻结切片**，以及 pair 完成率在 m1/m2 模式下能否抬升（本包裸腿结构的设计代价已实测可见）。在冻结窗口多段回放 + soak 门都说 GO 之前，allowlist 与账户绑定维持空置（README 红线 posture 不变）。

## Data notes

- Prices run on scaled integers (the §6.5 decimal-STRING wire rule); every
  trigger comparison is an exact integer comparison. Quotes are floored to
  the venue's 0.01 grid so a live POST never dies on an off-grid limit.
- One pair ATTEMPT per condition per round; a rejected attempt is not
  retried into the same book.
- Both books must be FRESH with a real ask (and visible ask depth) on each
  leg. A stale or phantom leg is not a half-signal, it is an outage.
- Round rollover retires every latch: conditions do not survive their round.
