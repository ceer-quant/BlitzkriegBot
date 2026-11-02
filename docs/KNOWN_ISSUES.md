# 已知缺口（公开）

仓库里曾经把细节放在不公开的 `dev-docs/KNOWN_ISSUES.md`。那个路径不在这棵树里，`docs/FEATURES.md` 的链接因此是死的。本文件只记录仍然影响「能不能跑 72 小时、能不能上真钱」的缺口，不是旧内部清单的复述。

## 1. 实盘成交和结算没有在这次改动里被验证

`FEATURES.md` 里 Polymarket 的真实下单/撤单记录，不等于成交捕获和结算链已经验收。dry-run、离线单测、以及 `docs/reports/cost-adjusted-edge-72h.md` 的费用扫描，都不能当成实盘盈亏。

默认 `DRY_RUN=true`（`.env.example` 与 `user_layer/configs/production-72h.env`）。100 USDC 的实盘文件是另选的 `production-live-100u.env`（issue #870）：下单前必须先过只读预检（余额、授权、链上成交和持仓）。预检通过也不等于策略可以启用，允许名单仍然是空的。

## 2. 当前策略没有扣费后的样本外 GO

见 [`reports/cost-adjusted-edge-72h.md`](reports/cost-adjusted-edge-72h.md) 和 issue #865。`pair_discount_arb` 1.3.0 / `single_leg_pair` 1.1.0 不在 72 小时启用集里。0.4.1 的 PF 早于 v1.3 的 maker 报价。

## 3. 系统风控以前默认全关

issue #864。正常启动现在读 `user_layer/configs/default.toml` 的 `[risk]`。`--no-config` 仍是全关，给测试用。

## 4. 盘口新鲜度与自动解冻

issue #866：默认预算 3500 ms，`0` 仍表示关闭，72 小时配置拒绝 `0`。
issue #867：自动解冻（含场地接单）必须安静满 10 分钟。手动冻结不会自解。

## 5. 72 小时 dry-run 怎么判

[`soak-72h.md`](soak-72h.md)。预检通过只表示「配置是干跑、限额开着、没有策略被启用」。它不是盈利许可。跑完之后用成交日志再打一次分：PF ≥ 1.1、最大回撤 ≤ $5、至少 30 笔平仓。
