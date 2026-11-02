# 72 小时干跑，以及 100 USDC 实盘入口

`.env.example` 里的 `DRY_RUN` 仍然是 `true`。下面两个脚本不会改这个文件。

## 干跑

在仓库根目录：

```bash
cargo build --release -p blitzkrieg-core
bash scripts/run-72h-dry.sh
```

这一条会：

- 强制子进程 `DRY_RUN=true` 和 `--mode dry`（父 shell 里即便导出了 `DRY_RUN=false` 也不会变成实盘）
- 只启用 `postonly_pair`，并加上 `--no-strategy-state`，避免旧的 `data/strategy-state.json` 把别的策略打开
- 使用 `user_layer/configs/production-72h.toml` 的亏损限额和 3500 ms 盘口新鲜度
- 把行情写到 `data/archive/events.jsonl`（256 MiB 轮转）
- 把成交写到 `data/trades/trades.jsonl`
- 模拟本金 100 USDC，单笔名义额上限 $2，股数 1–2

让它跑满 72 小时后用 Ctrl-C 停。打分：

```bash
node scripts/soak-72h-check.mjs --trades data/trades/trades.jsonl
```

通过条件：至少 30 笔平仓、利润因子 ≥ 1.1、最大回撤 ≤ $5。空日志是失败。

## 实盘

只有上一行退出码是 0 之后：

```bash
bash scripts/go-live-100u.sh data/trades/trades.jsonl
```

脚本先跑 `scripts/soak-72h-check.mjs`。检查失败就退出，不会启动内核。检查通过才用 `production-live-100u.toml` 启动：`DRY_RUN=false` 只加在这个子进程上，同时 `--mode live`，仍然只启用 `postonly_pair`。实盘成交写到 `data/trades/trades-live.jsonl`，不覆盖干跑日志。

`postonly_pair` 的回放结论仍是 NO-GO（见 `docs/reports/postonly-pair-replay.md`）。干跑没通过之前，不要跑 `go-live-100u.sh`。
