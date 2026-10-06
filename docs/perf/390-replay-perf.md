# #390 回放性能 — 调度与热路径优化存档

Issue: https://github.com/ceer-quant/BlitzkriegBot/issues/390（backtest nofast 维护调度 + 热路径剖析）

## 0. 语义红线

**输出字节级等价**：nofast 与 fast 两条路径的报告必须与修复前逐字节一致（除已记录的豁免字段：宿主计数 `calls/errors`、墙钟戳 `lastError/lastVenueError`、报告路径字符串、#388 合并引入的 `cashflows` 块与 feed 键、Lua 策略路径前缀、daily-loss 措辞）。

## 1. 基准命令（canonical replay）

```
core --no-config --engine --fee-model official --round-sec 900 --min-round-age 0 \
  --min-time-left 0 --slippage-ticks 1 --seed-balance 100000 \
  --enable-strategy pair_discount_arb --max-positions 10000 --max-positions-per-asset 10000 \
  --lua-strategy-dir <dir> --backtest corpus-15m-old.jsonl --backtest-report <out> \
  --backtest-tick-ms 50 --backtest-tail-ms 100        # fast 额外加 --backtest-fast
```

每次运行前 `rm -f data/positions/positions.daily-loss.json`（dayIndex 跨运行持久化会污染首日 roll 消息）。语料：`corpus-15m-old.jsonl`（348,631 事件，完整）；`corpus-head.jsonl`（20k 行，冒烟）。

## 2. 结果矩阵（Apple M-series，release 构建）

| 运行 | 语料 | 耗时 | tradesSha256 | 字节等价 |
| --- | --- | --- | --- | --- |
| base nofast FULL | 15m-old | **216m41s** (13001s) | `16ab61c00a8dcae6` | 基准 |
| step4 nofast FULL | 15m-old | 6m15.9s (375.90s) | 同上 | ✅ |
| **step5 nofast FULL** | 15m-old | **5m21.35s (321.35s)** | `16ab61c00a8dcae6` | ✅ vs base |
| step5 nofast head | head 20k | 3.62s | `fac858fc64543b9a` | ✅ vs base |
| base fast FULL | 15m-old | **1m8.21s (68.21s)** | `16ab61c00a8dcae6` | 基准 |
| **step5 fast FULL** | 15m-old | 1m9.22s (69.22s) | `16ab61c00a8dcae6` | ✅ vs base（噪声内 ±1.5%） |
| step5 fast head | head 20k | 3.38s | `fac858fc64543b9a` | ✅ vs base |

- **nofast 提速：216m41s → 5m21s = 40.5×**（step4 调度改造贡献 34.6×，step5 BookView 再贡献 1.17×）。
- **nofast/fast 剩余比值：321.35 / 69.22 = 4.64×**（step4 时为 5.5×）。

## 3. 改动内容

1. **Deadline 维护调度（backtest.rs jump_grid）**：nofast 不再"每 tick 全量维护"，改为 deadline 驱动 + wake-set 探针 + `has_forced_next_tick` 强制推进；fast 路径语义不变。
2. **BookView（exit_policy.rs / position.rs / service.rs）**：出场路径不再每仓位每 tick 构造完整 `OrderbookSnapshot`（两次 level 向量 clone + token String + 5 次 Decimal 归约），改为无生命周期 `Copy` 标量视图 `BookView::from_book`；`BookScalarView` trait 让生产代码与测试共享同一泛型出场管线。
3. 风控门禁测试改用 trait 对象适配（risk_gates.rs）。

## 4. 剖析（`sample <pid> 8 -file`，1ms 采样，fast FULL 稳态）

| 符号（含栈权重） | base nofast (5754 样本) | step4 fast (6316) | step5 fast (5716) |
| --- | --- | --- | --- |
| `luaV_execute`（Lua 策略求值） | 4420 (77%) | 2462 (39%) | 4093 (72%) |
| `call_callback`（宿主→Lua 回调） | 3421 (59%) | 1639 (26%) | 2729 (48%) |
| `book_to_lua`（盘口编组） | 1908 (33%) | 910 (14%) | 1487 (26%) |
| `Table::raw_set` | 2218 (39%) | 1202 (19%) | 1997 (35%) |
| `RawLua::create_string` | 1285 (22%) | 678 (11%) | 1139 (20%) |
| `engine_evaluate` | 5380 (94%) | 2506 (40%) | 4179 (73%) |

- nofast 剩余成本已收敛到 **每评估 Lua 地板**（≈348k 次真实策略求值 × pcall/编组开销）≈ 175–350s；fast 靠 50ms tick 批处理摊薄了引擎间隙。
- **下一杠杆（#390 后续）**：`book_to_lua` + `raw_set` + `create_string` 合计占求值路径 ~26–33%（含栈重叠，编组占 `call_callback` 约一半）——把逐 symbol `bk.book()` 改为批量回调（lua_runtime/bk_api，需与并行工作协调，本 PR 未动）预计可再砍 nofast 约 25–30%。

## 5. LuaJIT 评估结论（默认不实施）

- 宿主：mlua 0.12.1，`features = ["lua54", "vendored", "send"]`（`user_layer/lua_runtime/Cargo.toml:16`）→ **stock Lua 5.4**（lua-src 551.0.2），非 LuaJIT。
- 全部 10 个策略包扫描（`user_layer/strategies_lua/*/strategy.lua`）：**无** `goto`、`<const>/<close>` 属性、`& | ~ >> <<` 位运算符、`bit32/bit` 库、`string.pack/unpack`、`math.type/round`、`utf8.*`、`coroutine.close`；仅用 `//`（整除，LuaJIT 2.1 亦支持）；`luac -p`（5.5.1）全部通过。
- 结论：**语法层全部 LuaJIT 兼容**。但语义层有两个风险点：LuaJIT 无整数子类型（`//` 返回浮点，依赖整数十进制展开的 `d.m // POW10[...]` 需行为审计）；GC/字符串驻留行为不同。且剖析显示求值地板主要在 **宿主编组与 pcall 开销**（`call_callback` 48%），LuaJIT 解释器只能小幅改善这部分。
- 建议：维持 Lua 5.4；性能上优先做批量编组（§4），而非换 VM。
