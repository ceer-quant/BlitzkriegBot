# 72 小时 dry-run 清单

预检不启动内核，也不改 `DRY_RUN`。

```bash
node scripts/soak-72h-check.mjs --self-test
node scripts/soak-72h-check.mjs --preflight
```

通过预检表示：

- `DRY_RUN=true`（`.env.example` 和生产 env 都是）
- `[risk]` 九项限额是非零的，日亏绝对帽 $5，相对帽 10%（生产 env）
- 盘口新鲜度 3500 ms，不是 0
- `production-72h.allowlist` 里的名字都出现在 `docs/reports/cost-adjusted-edge-72h.json` 的 `allow` 里

当前 `allow` 是空的。启动命令带 `--no-strategy-state`，不会把旧的 `data/strategy-state.json` 里的策略重新打开。

机器上已经在跑的进程不用另起启动脚本。预检只读配置和成交日志。

## 跑完再打分

```bash
node scripts/soak-72h-check.mjs --trades path/to/trades.jsonl
```

GO 需要同时满足：

- 平仓至少 30 笔（空日志是失败，不是通过）
- 利润因子 ≥ 1.1（用日志里的净盈亏，费用应已含在净额里）
- 最大回撤 ≤ $5（与 `max_daily_drawdown_usd` 相同）

预检通过 + 没有成交日志 = 配置合格、盈利未证明。见 issue #868。

100 USDC 的实盘是另一份文件，不改变这里的 `DRY_RUN=true`。见 [`live-100u.md`](live-100u.md)。策略要先通过上面的 GO，才能写进那份允许名单。
