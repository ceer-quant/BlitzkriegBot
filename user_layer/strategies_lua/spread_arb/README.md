# spread_arb（Lua 版）— 趋势确认后的抄底策略

仓库的**官方 spread_arb**：Rust 参考实现（原 `user_layer/strategies/spread_arb` cdylib +
`strategy_logic` 算法）的**逐位精确 Lua 移植**。仓库不再提供 Rust 策略——本包就是
spread_arb。

## 语义（与 Rust 版逐位一致）

- **趋势确认**：滚动窗口（`trend_confirm_sec`，下限 10s）内 mid ≥ `trend_min_price`
  的样本占比 ≥ 0.8 且窗口展开 ≥ 90% → 该 token 确认；确认后 mid < `trend_broken_price`
  → 破位（内核据此撤销该 token 的挂单）。
- **入场**：只对已确认 token、只对**新鲜**盘口（`bk.book(sym).fresh == true`，内核的
  host-bound freshness 裁决）报价；resting bid = `trend_entry_factor × mid`
  （出厂 0.88——反逆向选择杠杆），钳制 `[0.05, 0.90]`、圆整到 0.01（**银行家圆整**，
  与 rust_decimal `round()` 一致）、不高于实时 best bid、严格低于 mid、封顶
  `trend_max_entry_price`。
- **高频过滤器**（默认全 0 = 关，经 manifest tunables 开启）：OBI 下限 / 点差上限 /
  回撤深度 guard（距趋势高点过远不接）/ 短窗反弹（turn filter——还在跌的盘口不挂单，
  防 maker 成交被下跌本身吃掉）。
- **算术纪律**：价格全部走缩放整数（与 rust_decimal 的 Decimal 语义逐位对齐，包括
  圆整的中点策略）；比值类单次除法用浮点——对市场数据的有理数，双精度与 28 位
  十进制在阈值比较上不可能分歧。

## 旋钮（manifest tunables = `bk.params()`）

出厂值 = Rust `SpreadArbConfig::default()`：

| 旋钮 | 默认 | 含义 |
|:--|:--|:--|
| `trend_min_price` | 0.55 | 趋势确认的价格下限 |
| `trend_confirm_sec` | 60 | 确认窗口（秒） |
| `trend_broken_price` | 0.35 | 破位地板 |
| `trend_entry_price` | 0 | 固定入场价（0 = 用 factor×mid） |
| `trend_entry_factor` | 0.88 | 入场折价系数 |
| `trend_max_entry_price` | 0.45 | 入场价上限 |
| `entry_min_obi` | 0 | OBI 下限（0 = 关） |
| `entry_max_spread_pct` | 0 | 点差上限 %（0 = 关） |
| `entry_dip_max_pct` | 0 | 距趋势高点最大回撤 %（0 = 关） |
| `entry_bounce_min_pct` | 0 | 短窗反弹下限 %（0 = 关） |
| `entry_bounce_window_sec` | 5 | 反弹观察窗（秒） |
| `entry_trend_window_sec` | 0 | 长记忆滑坡门的观察窗（秒，0 = 关；#176 移植） |
| `entry_trend_drop_pct` | 30 | 滑坡门阈值 %：mid 距趋势窗口高点跌幅 ≥ 此值 → 拒绝该 token 入场（门只在窗口开启时武装，阈值 0 不等于全拦） |

## 滑坡门（#176 移植，默认关）

`entry_trend_window_sec > 0` 时，每个入场评估额外量测：当前 mid 相对**长记忆窗口**
（`entry_trend_window_sec`，独立于确认窗）高点的跌幅百分比；跌幅 ≤ −`entry_trend_drop_pct`%
→ 拒绝入场（只挡入场，不影响已挂单——退出纪律归内核）。语义与
`mean_reversion.rs` 的 `trend_drop_pct`/`in_trend_slide` 一致（百分数刻度、窗口 ≤0 关、
历史不足两样本读零）。臂矩阵证据（Rust mean_reversion，#176）：600s 窗 / 30% 阈值
四窗一致 +$23.7/4h；Lua spread_arb 冻结语料复测结论见合并本改动的 PR 描述——
**测得为改善才改出厂值，否则默认保持关**。

注意：内核的 `--spread-arb-*` CLI 旋钮**不触达 Lua 策略**（那是 dylib on_params
通道）——本包的旋钮唯一来源是这份 manifest。影子进化/可进化旋钮声明是 dylib 面
的能力，Lua 栈暂未接（生产进化本就关闭）。

## 验证

冻结语料（4 窗，sha256 钉定）上的 A/B 逐位对比：本包与被删除的 Rust cdylib 在
closed / win rate / net USD 上逐字一致，且与 `exit-economics-check.mjs` 的
BASELINE（`75fd8140`）逐字一致——证据见合并本包的 PR。经济门禁的 fixture 就是
本包。
