# HFT 策略影子优化报告（spread_arb）

- 日期：2026-09-14
- 范围：`spread_arb` 策略（Rust 核心实际成交路径，`engine.rs` + `signal.rs::evaluate_spread_arb` + `exit_policy.rs`）
- 方法：**纯离线、确定性、不开 Live、不重启在跑进程**。复用核心自带的影子回放/
  walk-forward（与生产共用同一份 `exit_policy`，回测不会与实现漂移）。
- 结论一句话：**策略源码默认参数（SL12/trail8）本身已被样本外验证为优；"表现不忍直视"
  的直接原因是在跑的 dry 内核是旧二进制，仍执行旧的约 50% 宽止损。** 本次不改交易阈值，
  只修复"看不到真实生效参数"的工具与可观测性缺口，并用严格冻结 holdout 固化证据。
- 部署动作（重启换新二进制）受 Soak 红线保护，已记入 `DECISIONS_PENDING.md` **D-10**，等待授权。

---

## 1. 现状（在跑的 dry 成交）

数据源 `data/trades/trades.jsonl`（持续增长；复盘时点 122 笔已平仓）。

| 出场原因 | n | 毛利 $ | 费用 $ | 净利 $ | maker 进场 | maker 出场 |
|---|---:|---:|---:|---:|---:|---:|
| stop_loss | 47 | −55.40 | 6.21 | **−61.61** | 0 | 0 |
| trailing_stop | 68 | +63.50 | 10.13 | **+53.37** | 0 | 0 |
| take_profit | 4 | +6.20 | 0.58 | +5.62 | 0 | 0 |
| time_exit | 3 | +0.40 | 0.43 | −0.03 | 0 | 0 |

- 盈利一侧（trailing + TP）净 **+$59**；亏损一侧（stop_loss）净 **−$62**。
  **盈亏结构本身成立，被宽止损的单笔大亏吃掉。**
- `wasMakerEntry=0/122`、`wasMakerExit=0/122`，每笔都付 taker 费；
  进场 taker 费约占名义 1.7%（p≈0.44），双边全 taker 约 3.4%，高于约 2% 的单笔优势。
- stop_loss 的实际净亏集中在 **−17%～−58%**（例：0.45→0.22 = −53.6%，0.41→0.20 = −53.8%），
  **没有一笔落在源码默认止损 12% 附近。**

## 2. 根因：在跑的是旧二进制，不是当前源码

- 进程 73052（node 72966 于 09:22 拉起）持有旧 inode 二进制：
  `lsof` 显示 12,884,112 字节；当前 `target/release/blitzkrieg-core` 为 12,951,024 字节，
  mtime 17:05。macOS 会让运行中的进程继续用旧 inode。
- 当前源码 `ExitConfig::default()`（`exit_policy.rs`）**已经是**
  `stop_loss_pct=12 / min_trail_pct=8 / trailing_min_high_pct=15`（与 D-2 记录一致）。
- 旧进程成交的 −17%～−58% 止损幅度，正对应**旧的宽止损（约 50%）**时代。
- 因此：**−$9.89（复盘早期口径，122 笔时约 −$2.7）是旧策略的结果，不能用来否定当前源码。**
  修复部署后，dry 才会真正交易 SL12/trail8。

## 3. 离线验证：当前源码参数在样本外成立

数据集 `data/backup-20260914-001139/shadow/positions.jsonl`（62 笔带完整路径、按时间排序；
唯一足够长的路径数据集，更早期 09-12 的文件仅 4–6 笔可用，不足以做样本外切分）。

### 3.1 严格冻结 holdout（本次新增，防止自适应 walk-forward 对单段行情过拟合）

做法：在**前半段**上为每个候选参数选一次最优，然后**冻结**应用到**后半段未见数据**
（不逐笔重优化）。见 `shadow::frozen_holdout`。

| 切分 | 参数 | 训练 $（胜率） | **样本外测试 $（胜率）** |
|---|---|---:|---:|
| 50/50 | old-SL50/trail10 | +11.13（65%） | **+1.81（65%）** |
| 50/50 | **shipped-SL12/trail8** | +14.50（61%） | **+10.07（65%）** |
| 50/50 | SL12/trail10 | +14.00（61%） | +9.28（61%） |
| 50/50 | SL15/trail8 | +13.71（61%） | +8.15（65%） |
| 60/40 | old-SL50/trail10 | +11.65（62%） | **+1.30（68%）** |
| 60/40 | **shipped-SL12/trail8** | +16.11（62%） | **+8.47（64%）** |

要点：

1. **shipped SL12/trail8 在两个切分上都是"训练最优"，且冻结到未来仍最优**
   （PF≈2.3–2.4），不是单段运气。
2. 旧 SL50 在样本外塌到 +$1.8 / +$1.3（PF≈1.1，基本白做）——与在跑结果吻合。
3. 邻近格（SL10/SL15、trail6/10）样本外均**不优于** shipped；SL12/trail6 仅在 25 笔上
   多约 +$1（噪声级）且更偏离 Node 参照，**不追逐**。
4. 全样本（乐观上界）：shipped +24.58 vs old +12.95；自适应 walk-forward 56 笔 OOS +22.46。

原始数值：`docs/reports/data/hft_exit_holdout.csv`。

### 3.2 方向 / 入场价带不做硬编码

早前两段数据对方向与价带的结论**互相矛盾**（一段 DOWN 亏、另一段 DOWN 赚；.44–.45
一段赚一段亏），属行情依赖，不稳健。冻结 holdout 也未显示需要在进场侧加方向/价带过滤，
故**不引入**这类易过拟合的硬编码（遵守"证据不足不动业务逻辑"）。

## 4. maker 0 成交：机制已定位，但本次不改 maker 超时

- 下单恒为 `Buy + MakerThenTaker`，挂单价 = `round2(min(mid*0.98, best_bid, 0.45))`；
  dry 下 maker 仅在 `best_ask ≤ 挂单价` 时成交，否则 **5000ms 后升级为 taker**
  （`service.rs` maker→taker 升级；taker 在 dry 下按挂单价立即全成）。
- **人群差异**是关键：near-miss（被门挡住、多为逆势回落）记录的后续盘口里，挂单价
  5s 内被扫到约 54%；而**真正在趋势确认后吃进的单**，ask 单边上行不回挂单价。
  对 62 笔已成交路径用 `mid ≤ 入场价`（maker 成交的必要条件）给出**上界**：
  5s ≤58%、20s 63%、60s 74%——5s→20s 只多约 5 个百分点的可能成交，却要让趋势单多等。

| 人群 | 价格口径 | 5s | 10s | 20s | 60s |
|---|---|---:|---:|---:|---:|
| near-miss（被挡） | 精确含 cap | 54% | 66% | 71% | 81% |
| near-miss（被挡） | 不 cap | 68% | 77% | 78% | 86% |
| 已成交 62 笔（**上界**） | 精确含 cap | ≤58% | ≤61% | ≤63% | ≤74% |

原始数值：`docs/reports/data/hft_maker_fill.csv`。

- 结论：延长 maker 超时是**人群依赖**的杠杆，在真实成交人群上增益小、且会推迟趋势进场，
  证据不足以改；保持 5000ms。费用问题的根治仍以"先让正确的 SL12/trail8 上线"为主
  （盈利侧已能覆盖 taker 费），maker 参与率留待上线后用**同口径**新影子数据再评估。

## 5. 本次改动（只动工具/可观测性，不动交易阈值）

1. `core/blitzkrieg_core/src/shadow.rs`
   - **订正 walk-forward 网格**：把真实在跑的 `shipped-SL12/trail8` 纳入并正确标注；
     旧网格误把一个 SL15/trail10 点标成 "shipped"，且**根本没有 trail8 单元**——
     这正是离线研究长期"没给真实生效参数打分"、旧宽止损二进制无人察觉的缺口。
   - 新增 `frozen_holdout()`：选一次、冻结应用到未来窗口的严格样本外评估，并配单测。
2. `core/blitzkrieg_core/src/main.rs`：`--replay` 输出新增 50/50 与 60/40 冻结 holdout 表。
3. `core/blitzkrieg_core/src/ipc/server.rs`：启动时打印**生效的**风险/出场参数
   （`exit tuning: stop_loss=..% take_profit=..% trail_min=..% … maker_timeout=..ms`），
   旧二进制/错配置无法再静默上线。
4. 证据数据：`docs/reports/data/hft_{exit_holdout,maker_fill,live_exit_reasons}.csv`。

未改：任何进场/出场阈值、方向、maker 超时、仓位、Live 开关。

## 6. 门禁（全绿，DryRun）

- `cargo build --release`：通过。
- `cargo test --release`：blitzkrieg-core **102 passed**（含新增 frozen_holdout 测试）。
- `npm run typecheck` / `npm run build`：通过。
- `scripts/secret-scan.sh`：无密钥。
- `node scripts/cycle-check.mjs`：**PASS**（确认趋势→挂单→maker 成交→持仓→估值全链路），
  其启动日志已显示新二进制 `stop_loss=12% … trail_min=8%`。

## 7. 待办（需用户）

- **D-10**：是否授权用新二进制重启 dry 内核（不开 Live），重启后核对启动日志的
  `exit tuning: stop_loss=12%`。授权前不重启。
- 上线后用同口径影子数据复盘 maker 参与率；若趋势人群的零费率成交确实可抓，再单独立项
  评估 maker 超时/挂单方式（届时需带 bid/ask 的新路径数据，当前在跑版本未记录路径）。
