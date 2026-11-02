# 100 USDC 实盘配置（显式选用）

本金按 100 USDC 算。这份配置不会变成默认启动。`.env.example` 里的 `DRY_RUN` 仍然是 `true`。只有 `user_layer/configs/production-live-100u.env` 把 `DRY_RUN` 写成 `false`，而且要你自己 source，或者跑下面的 `--arm-orders`。

相关 issue：#870。

## 限额

和 72 小时配置、`default.toml` 是同一组数：

| 限额 | 值 |
| --- | ---: |
| 全局停机 | $8 |
| 日亏 | $5 |
| 单笔止损上限 | $0.50 |
| 总敞口 | $20 |
| 盘口新鲜度 | 3500 ms |

单笔名义额：绝对上限 **$2**，同时 `BK_MAX_ORDER_NOTIONAL_PCT=2`（100 USDC 的 2% 也是 $2）。进场预算 `BK_SIZE_PCT=1`，也就是 $1。股数带是 1 到 2，按 $1 的价格也不会越过 $2。

策略允许名单是空的。有策略通过 soak 门禁（PF ≥ 1.1、最大回撤 ≤ $5、至少 30 笔平仓）之前，不要往 `production-live-100u.allowlist` 里加名字。

## 只读预检

预检不签名、不读私钥、不启动会下单的进程。它用 `eth_call` 读 Polygon 上的 USDC.e 余额和三个交易所合约的授权，再用公开的 data API 读持仓和成交，跟本地 `data/positions/positions.jsonl`、`data/trades/trades.jsonl`（如果存在）对账。

```bash
node scripts/live-100u-preflight.mjs --self-test
node scripts/live-100u-preflight.mjs --config
POLYMARKET_FUNDER_ADDRESS=0x... node scripts/live-100u-preflight.mjs
```

通过条件：

- USDC.e 余额在 90 到 110 之间（100 的 ±10%）
- CTF Exchange、Neg Risk Exchange、Neg Risk Adapter 的授权都不少于余额
- 链上持仓和本地持仓一致（0.01 股以内）
- 有本地成交日志时，交易哈希集合必须和链上一致
- 没有本地日志时，链上必须是空仓且没有成交；否则拒绝
- 成交页若满 500 条，视为没读完，拒绝

任何一项失败，订单保持不允许。

## 武装下单

只有预检退出码是 0 之后：

```bash
bash scripts/live-100u.sh --arm-orders
```

子进程是 `--mode live --no-strategy-state`，不带 `--enable-strategy`。这仍然不是盈利许可。没有策略通过 soak 之前，这个进程没有策略单。
