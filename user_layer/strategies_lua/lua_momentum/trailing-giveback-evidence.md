# lua_momentum trailing giveback — 修前/修后实证数据（issue #1076）

## 测试方法

使用 `scripts/momentum-trailing-sweep.mjs`，在 pin 的 4 个 frozen-corpus 窗口上跑 `--backtest`（dry，release 内核 `target/release/blitzkrieg-core`），每窗口双跑取一致结果，按窗口汇总 netPnlUsd/WR/closed。

测试命令：
```
node scripts/momentum-trailing-sweep.mjs --only <window>
```

## 修前（main，ceer/main 0d6169cc）

策略：`user_layer/strategies_lua/lua_momentum` 出厂版本（trailing giveback 按净 proceeds `(bid − fee)` 计算）。

| 窗口 | closed | wins | WR | netPnlUsd |
|------|--------|------|-----|-----------|
| trend-20260919T1000Z | 26 | 6 | 23.08% | -21.1152 |
| range-20260919T1600Z | 27 | 13 | 48.15% | -6.5758 |
| trend-20260920T2100Z | 26 | 8 | 30.77% | -15.8140 |
| range-20260920T2300Z | 25 | 15 | 60.00% | 0.2167 |
| **total** | **104** | **42** | **40.4%** | **-43.2883** |

## 修后（fix/1076-trailing-giveback，d61d1f1f）

策略：trailing giveback 按高水位 GROSS best_bid 回撤 `((peak_bid − bid)/entry)`，与内核让回口径 `highPnlPct − netPnlPct` 一致。

| 窗口 | closed | wins | WR | netPnlUsd |
|------|--------|------|-----|-----------|
| trend-20260919T1000Z | 26 | 6 | 23.08% | -21.1152 |
| range-20260919T1600Z | 27 | 13 | 48.15% | -6.5758 |
| trend-20260920T2100Z | 26 | 8 | 30.77% | -15.2748 |
| range-20260920T2300Z | 25 | 15 | 60.00% | 0.2167 |
| **total** | **104** | **42** | **40.4%** | **-42.7491** |

## 差异

| 窗口 | ΔnetPnlUsd | 说明 |
|------|-----------|------|
| trend-20260919T1000Z | 0.0000 | 无 trailing_stop 触发，结果相同 |
| range-20260919T1600Z | 0.0000 | 无 trailing_stop 触发，结果相同 |
| trend-20260920T2100Z | +0.5392 | 修后 net 少亏 $0.54，让回更早更准确 |
| range-20260920T2300Z | 0.0000 | 无 trailing_stop 触发，结果相同 |
| **total** | **+0.5392** | 总计少亏 $0.54 |

## 结论

1. **方向正确**：修后在所有窗口不劣于修前，趋势窗口（trend-20260920T2100Z）净亏减少 $0.54，说明按 GROSS bid 高水位回撤能更早触发出场，避免更大让回。
2. **未放宽记账口径**：内核口径仍为 `balance − opening_equity`，`reserved` 不计入；仅改变了 trailing 触发的基准（从净 proceeds 改为 GROSS bid），触发更早而非更晚。
3. **样本受限**：4 个窗口共 104 笔成交，trailing_stop 触发样本稀少（多数窗口无触发）。如需更大样本，需扩展 corpus 或在 live 上追踪。
4. **让回问题未完全消解**：issue 中提到的“20.6pp vs 15pp”主要来自无 bid 时的 withheld 出场缺口（kernel 语义，不可由代码消除），仅靠调整 giveback 口径无法消除该部分让回。如回放后全样本让回均值仍超 18pp，按 issue 兜底用数据回报，而非代码端静默放宽阈值。

## 复现命令

```bash
# 修前（main）
git checkout main
node scripts/momentum-trailing-sweep.mjs

# 修后（fix/1076-trailing-giveback）
git checkout fix/1076-trailing-giveback
node scripts/momentum-trailing-sweep.mjs
```
