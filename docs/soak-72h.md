# 72 小时 dry-run 清单

预检不启动内核，也不改 `DRY_RUN`。

```bash
node scripts/soak-72h-check.mjs --self-test
node scripts/soak-72h-check.mjs --preflight
bash scripts/soak-72h.sh --print-argv
```

通过预检表示：

- `DRY_RUN=true`（`.env.example` 和生产 env 都是）
- `[risk]` 九项限额是非零的，日亏绝对帽 $5，相对帽 10%（生产 env）
- 盘口新鲜度 3500 ms，不是 0
- `production-72h.allowlist` 里的名字都出现在 `docs/reports/cost-adjusted-edge-72h.json` 的 `allow` 里

当前 `allow` 是空的。启动命令带 `--no-strategy-state`，不会把旧的 `data/strategy-state.json` 里的策略重新打开。

## 开跑（仍然是 dry）

只有在预检通过、并且你明确要占着一个前台进程时：

```bash
bash scripts/soak-72h.sh --start
```

这不会把 `DRY_RUN` 设成 `false`。真钱上线不走这个脚本。

## 跑完再打分

```bash
node scripts/soak-72h-check.mjs --trades path/to/trades.jsonl
```

GO 需要同时满足：

- 平仓至少 30 笔（空日志是失败，不是通过）
- 利润因子 ≥ 1.1（用日志里的净盈亏，费用应已含在净额里）
- 最大回撤 ≤ $5（与 `max_daily_drawdown_usd` 相同）

预检通过 + 没有成交日志 = 配置合格、盈利未证明。见 issue #868。
