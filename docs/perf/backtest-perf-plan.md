# 回测性能剖析与优化排序 — spread_arb 全量回放 26 小时归因

状态：分析完成（2026-10-07）。本文档是 spread_arb book 语义修复后全量回放从 ~10 分钟量级退化到 ~26 小时的定量热点归因与优化排序。**零代码修改**——所有改动建议供后续 PR 落地。

采样对象：`core-bookfix-fixed`（PID 16196，构建自 bk-wt-bookfix@fb74c65f 工作树，UUID `9F736575-ACC9-3E18-B606-250C83C86E18` 双向核验）。语料 356,749 事件，`--backtest-fast --backtest-tick-ms 50`，采样点位于回放约 49% 处（事件 173,991 附近）。

---

## 1. 剖析方法与样本

- `sample 16196 10`（1ms 采样）两轮：`/Volumes/Hard Disk/bk-session/spread-arb-replay.sample.txt`（6533 主线程样本，16:39）与第二轮（6316 样本，16:47，存 bk-agentperf）。两轮热点结构一致（`Core::tick+14128` free 分别 2126/6530 与 2348/6316），**无漂移**。
- 二进制与 rlib、bitcode 均无 DWARF（release profile 无 debug 选项，无 dSYM），`atos` 无法给行号。改用反汇编对照：`Core::tick` 文件 VA `0x10015bd64`，运行时基址 `0x104CCBD64`（sample 内 tick 地址减偏移反推，与 nm 符号表一致），sample 的 `+NNNNN` 即 tick 内字节偏移。
- 每个热点偏移用 `objdump -d` 找到调用点目标符号（Rust mangle 可读），再对照源码行，归因闭环。

## 2. 热点归因表（样本 #1：主线程 6533 样本）

主线程 100% 在 `run → jump_grid → step`；`step` 内 83.3% 在 `Core::tick`（5444/6530），16.4% 在 `drain`（回放 channel 排空，属回放驱动固定成本）。tokio 的 10 个 worker 线程全部空闲（cvwait/kevent）——多核完全未利用。

| # | 热点（tick 内直属样本） | 样本 | 占全样本 | 源码位置 | 机理 |
|---|---|---|---|---|---|
| 1 | `tick+14128` → `_xzm_free_main` 2126 + `tick` 自身 548；另有 `tick+13532` malloc 95 | 2774 | 42.5% | service.rs:8567-8576（kline preview 循环） | 每 250ms 虚拟节拍的 preview pass：`kline_current_bars()` 克隆全部活跃 Kline（engine.rs:689 → aggregator.rs:216 `values().cloned().collect()`），Kline 含 7 个 String/Decimal 的 78+ 字节结构，事件在 channel 中两次构造两次销毁 |
| 2 | `tick+13128` malloc 490 + `tick+13148` memmove 365 | 855 | 13.1% | service.rs:8567-8576 | 同上：preview 循环的 Vec 分配 + Kline 逐元素 memmove 克隆 |
| 3 | `tick+13836`/`tick+13608` → `hash_one(String, KlineInterval)` + SipHash13 | 646+ | 9.9% | service.rs:8568-8574 | 每轮 pass 为每个 bar 构造 `(String, KlineInterval)` 键（`bar.symbol.clone()`）查 `kline_preview_last_ms`，get+insert 两次全量 SipHash（hash_one 自身 452 样本 + SipHash write 465 样本，跨项部分重叠） |
| 4 | `tick+14360` → `UnboundedSender::send` | 272 | 4.2% | service.rs:8575 `self.emit(Event::KlineUpdate { kline: bar })` | 事件进 tokio unbounded channel：find_block + memmove + 节点分配 |
| 5 | `tick+13708`/`tick+13956` → `platform_memcmp` | 144+40 | 2.8% | service.rs:8570-8572 | HashMap 探测中 String 键的字节比较 |
| 6 | `tick+14232`/`tick+14344`/`tick+13552` → memmove 直属 | 408+ | 6.2% | service.rs:8574-8575 | `KlineUpdate` 事件构造（Event 枚举按值移动 Kline，Event 尺寸数百字节）+ map insert 的 KV 移动 |
| 7 | `step+300` → `drain` 子树 | 1077 | 16.5% | backtest.rs:825-871 | 回放 channel 侧：`Rx::pop` 的 memmove、事件 drop_glue（含 KlineUpdate 的 String 释放）、`find_block` |
| 8 | Lua 求值（`engine_evaluate` → `luaV_execute`） | ~9 | 0.2% | — | 对照 docs/perf/390-replay-perf.md：pair_discount_arb 时代 Lua 占 72%，如今完全让位 |

**归因结论**：tick 直属未归因样本（原 ~480/6530）经反汇编全部落入 service.rs:8560-8579 的 E29 kline preview 循环。该循环 + 其 channel 事件的生命周期（含 drain 侧 KlineUpdate drop）合计约占主线程样本 **60%+**；其中 malloc/free 家族 38.4%（xzm_free_main 2126+548+drain 侧 98、tiny malloc 394+57 等）的最大单一 owner 就是 Kline 的克隆与丢弃。

## 3. 决定性 A/B 实验（预览通道关闭）

在隔离实验树（bk-agentperf/run/exp-tree，独立 CARGO_TARGET_DIR 与 cwd）做最小改动：给 E29 preview pass 加 `replay_kline_preview_enabled` 配置门控（默认 true 保持现行为；replay 下置 false 跳过整个 pass），env 钩子 A/B。

| 切片 | preview on | preview off | 加速 | 报告 sha16（前 16 位） |
|---|---|---|---|---|
| 语料头部 3k 事件 | 3893ms | 979ms | 4.0× | 双方 `a73ba4fd62e3b93b` ✅ |
| **语料中部 30k（170,000-200,000 事件，对齐采样点的满载状态）** | **434s** | **11s** | **39.5×** | 双方 `c7704fea2c6ab6d7` ✅ |

中部切片正是采样点（49%）所在的语料区段：bar 多、仓位满、preview pass 每轮要克隆几十个 Kline。**该区段 preview pass 占回放墙钟 ~97%**。头部切片只有早期少数市场，所以只有 4×——切片选取对结论影响巨大（方法论教训，记录在案）。

**外推**：26h 全量回放 ÷ 39.5 ≈ **40 分钟**。与样本权重（tick 83% + drain 17%，其中 preview 家族占大头）一致。

方法论纠错记录：第一轮 A/B 因 zsh `env ... time ...` 解析错误导致 cargo build 实际未执行、env 钩子没进二进制（`grep -c -a BK_PERF_PREVIEW` = 0 时发现的）。已建立检查项：**每次编译后必须验证钩子符号/字符串进入二进制**，否则 A/B 无效。另一次教训：daily-loss 状态文件跨运行持久化污染了同 cwd 二跑的报告（"day 20673 closed at"字样），A/B 必须用全新 cwd（与 #390 文档第 1 节的告诫一致）。

## 4. 语义安全性论证（为何 replay 下关 preview 是输出不可见的）

1. **回放侧不消费**：backtest.rs:825-871 `EventBacktester::drain` 的 match 对未列出事件走 `_ => {}`——`Event::KlineUpdate` 被直接丢弃，不进 trades/order_status/fills 任何统计。
2. **唤醒调度器不含它**：service.rs:1451-1501 `next_replay_wake_ms`（#390 的 deadline 跳跃机制）枚举的唤醒源（timing/eval-fallback/exit/escalation/expiry/query/redemption/cooldowns/inflight/breaker/audit/day-roll）没有 preview 截止时间——#390 设计上已把 preview pass 视为对输出不可见的副作用。
3. **通道隔离**：backtest.rs:666 `unbounded_channel` 是回放专属 pair，`tx` 只有 drain 一个消费者。
4. **关闭 pass 副作用面**：`kline_preview_last_ms` 的 insert 变少（该 map 无其他读者）与 `fast_last_kline_ms` 停更（仅被本 pass 读取）——两者都不进任何输出路径。
5. **实测**：两档切片报告 sha256 逐位一致（§3 表）。

正式落地的验收基线仍按任务纪律执行：pair_discount_arb + single_leg_pair 全量 v2 回放 trades 块逐字节一致（各 ~10 分钟）+ exit-economics 4 窗口逐位一致（13/30.77/+5.2070、12/16.67/-0.2262、6/16.67/-0.3556、13/0/-11.0845）+ spread_arb 修复后回放与 26h 基线 JSON 逐位一致。

## 5. 优化排序（性价比 = 预期加速 / 实现风险）

### P0 — replay 模式跳过 E29 kline preview pass

- **证据**：§2 表全部条目 + §3 的 39.5× 实测 + §4 的五重安全论证。
- **改法**：`CoreConfig` 增加字段（如 `replay_kline_preview_enabled: bool`，默认 `true`）；service.rs:8560-8562 的 `kline_pass` 判定在 `replay_sched()` 为真时同时要求该字段为 false 才关（或直接在 backtest 路径构造 config 时关）。改动 ≤10 行，集中在 `tick` 开头一处。更彻底的变体：`drain`/report 完全不需要 KlineUpdate 时，backtester 直接不创建 tx 或 send 前 `matches!` 过滤——但最小 diff 是门控。
- **预期加速**：保守 10×（对全量均值打折，头部区段只有 4×），乐观 30-40×（采样点区段实测）。26h → 40min-2.6h。
- **回归红线**：§4 末段的全部逐位一致验收；特别要跑一个含 KlineUpdate 的 live/IPC 冒烟（preview 对 live 面板仍是功能）。
- **风险**：极低。唯一语义面在 live 模式，字段默认值保证 live 不变。

### P1 — preview pass 保留时的三个微改造（若 P0 因故不采纳，或 live 面板也要省）

按收益排序（样本占比见 §2）：
1. **键预计算/interest 缓存**（~10-13%）：`kline_preview_last_ms` 键从每轮 `(String, KlineInterval)` clone+双 SipHash 改为——Kline 聚合器已按 bar 维护（aggregator.rs:216 的 `self.bars` BTreeMap），把"上次推送时间"直接挂在聚合器的 bar 值上，或用 `(u64 symbol_hash, u8 interval)` 键。消灭 #3 与 #5。
2. **克隆改借用/事件瘦身**（~25-30%）：`kline_current_bars` 返回 `Vec<&Kline>` 或只返回 `(symbol, interval, open/high/low/close, is_closed)` 的 Copy 视图（#390 已对 BookView 做过同型改造，有先例与测试范式）；`Event::KlineUpdate` 若必须保留完整 Kline，改为 `Arc<Kline>` 或瘦身枚举，消灭 #1/#2 的大部分与 #6。
3. **节拍放宽**：`FAST_KLINE_MIN_MS`（service.rs:1412，现 250ms）在 replay_sched 下本可放宽——但因 P0 已整体跳过，此项只对 live 有意义，**不动**。

- **预期加速**：三项合计把 preview 家族成本砍 ~70-85%，对应全量回放 ~1.5-2×；但均被 P0 包含，仅作 P0 不可行时的替代路线。
- **回归红线**：同上（live 路径必须保持事件字段不变，瘦身只在 replay 内部视图做）。

### P2 — drain 侧事件排空成本（16.5%，backtest.rs:825-871）

KlineUpdate 事件在 drain 侧的 pop memmove + drop_glue 也占一份。P0 落地后此成本自动消失大半（事件不再产生）；剩余的 OrderUpdate/Fill clone 是报告输入，**语义承载，不动**。

### P3 — 明确不改的项

- **`--backtest-tick-ms 50` 粒度**：#390 的 jump_grid 已经把空转 tick 跳掉（next_replay_wake_ms 探针），样本中 `Backtester::run+368`/`jump_grid+1352` 自身仅占 ~0.2%——tick 粒度不是热点，改语义（如 500ms）会动所有时间戳，**收益趋零而风险巨大，不动**。
- **Decimal 换 f64**：exit/结算语义逐位一致红线直接排除。
- **tokio 多线程利用**：worker 全闲是架构现状（回放是单线程状态机），并行化是重写不是优化，不在本轮。
- **Lua 桥（book_to_lua 等）**：本轮样本 0.2%，pair_discount_arb 时代的 72% 已被 #390 的调度解决；spread_arb 的瓶颈完全不在 Lua。

## 6. 复现命令

```bash
# 采样（回放进行中，只读）
sample 16196 10 -file /Volumes/Hard Disk/bk-agentperf/sample-N.txt

# 符号定位（二进制无 DWARF，走反汇编）
nm -n <bin> | grep '4Core4tick'      # 文件 VA 0x10015bd64
objdump -d --start-address=<VA+off-0x100> --stop-address=<VA+off+0x200> <bin>

# A/B 实验（bk-agentperf/run/exp-tree，env 钩子 BK_PERF_PREVIEW=0/1）
# 必须全新 cwd（daily-loss 状态跨运行持久化会污染报告）
BK_PERF_PREVIEW=0 <exp-bin> ... --backtest <slice> --backtest-report <out> --backtest-fast --backtest-tick-ms 50 --backtest-tail-ms 100
```

实验产物与原始数据：`/Volumes/Hard Disk/bk-agentperf/`（sample-2.txt、run/corpus、run/mid-on、run/mid-off、PROGRESS.md）。
