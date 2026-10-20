# Issue #540 监控项评论草稿（可直接粘贴）

> 以下分隔线内为评论正文。粘贴时保留 Markdown。锚点均在 feat/arb-batch-precheck
> worktree 验证（2026-10-20）；`service.rs` = `core/blitzkrieg_core/src/service.rs`，
> `host.rs` = `core/blitzkrieg_core/src/market/host.rs`。详细核对步骤见
> `docs/arb-arch-acceptance.md`。

---

按验收标准 3/4，把本 Issue 的升格触发条件落成监控项。每条给「核对命令/数据源 + 判定阈值 + 升格动作」。数据源：订单日志 `data/orders/orders.jsonl`（`TrackedOrder` 行，含 `internalKey`/`avgFillPrice`/`filledSize`，service.rs:202 默认路径）、成交 `data/trades/trades.jsonl`、stderr（core 进程）、审计 `reconcile.jsonl`。

**监控 1（⑥-a，P0 升格）：风控读到错误 venue 余额放行的超额订单。**
机制：两 venue bridge 各自 seed/realign 同一本 active 账簿（polymarket live.rs:91/440、kalshi executor.rs:265/480 → host.rs:462-494 → `set_balance` service.rs:2774-2796，单格 `venue_free` :1548），风控 `check_with_equity`（place_gated :11724-11725、place_pending :7767-7768）的裁决基准因此在两笔真实余额间震荡。
核对：对每笔超额嫌疑订单 O——`jq 'select(.orderId=="O")' data/orders/orders.jsonl` 取名义价值；`grep 'core: venue cash reconcile'`（host.rs:480）与 `jq 'select(.ledgerRealigned != null)' reconcile.jsonl`（service.rs:8267，30s 间隔）找 O 提交前 ~10s 的余额改写现场；用 `data/ledger/<account>.journal.jsonl` 复原 O 所落 venue 的真实余额。
阈值：复原余额 < O 名义价值，且改写来自另一 venue 的 realign（限「无挂单改簿」区间；有挂单时 host.rs:487-489 只报 drift 不改簿，不算）。
升格动作：**升格 P0**，凭对账工单在 RFC 定稿前插队裁决。

**监控 2（⑥-b，P0 升格）：asset 冲突静默放行修复后仍现结算不收敛。**
机制：`unified/ledger.rs:181-185` 的 `asset_conflict` 在任一侧 asset 缺失时为 `false`（静默放行），只剩标题相似度兜底。
核对：unified 映射工单 + `data/trades/trades.jsonl` 两腿结算行。
阈值：修复 PR 合入时间戳**之后**，第一笔「同一映射两腿结算合计 ≠ $1」工单。
升格动作：**升格 P0**。

**监控 3（②③，P0 升格）：滑点侵蚀 >20%。**
归因键（order log 按 `internalKey`，三种前缀别混）：裸腿 flatten = `flatten:{position_id}`（manual exit 路径 service.rs:12564/:12618，orphan sweep 入口 :10016）；orphan chase = `pair-chase:{naked}:{missing}:{slot}`（:10097，经 `place_escalated` :10145 下发）；maker→taker escalation = `{id}:escalated`（:7936/:13822，常态开仓成本，仅当对应批发生 ②/③ 失败时并入，防重复计数）。
核对：`jq -r 'select(.internalKey|startswith("flatten:") or .internalKey|startswith("pair-chase:")) | [.internalKey,.avgFillPrice,.filledSize] | @tsv' data/orders/orders.jsonl`；每笔 taker 成本 = `|avgFillPrice − 参照价| × filledSize + taker 费`（flatten 参照价 = positions.jsonl 的 costUsd/shares；chase 参照价 = OME 历史死腿原价）。
阈值：Σ(taker 成本) ÷ **同期 `cross_venue_arb` 已实现收益**（trades.jsonl 同窗口 closed trades，不含浮盈）> 20%。触发频率本身（次/百对）同表上报。
升格动作：**升格 P0**。

**监控 4（方案 1 → 2a 强制升级，三条任一命中）：**
(a) 操作员工单「面板已启用、后台每轮挂出→退休循环」——数据源 = stderr「place timed out … order retired」（service.rs:9744-9748，30s 常量 :14586）与 `market.credentials.status`（ipc/server.rs:1436，ready=false）同现、同策略 ≥2 轮；动作 = 2a（引擎 scope 过滤器 pair 化 + `virtual_batch_fits` :6926 扩凭证预检）立项为必做。
(b) 第一笔凭证缺失导致的裸腿 flatten 滑点损失（对账证据）——`flatten:` 成交行 ∩ 该 venue 插件 ready=false 时间窗；动作同上。
(c) 新增第二个跨所策略需重打面板补丁——`user_layer/strategies_lua/` 新 `atomic_batch` 包 + P1-a 面板第二次 diff；动作同上。

**监控 5（方案 2a → 方案 3 强制升级，三条任一命中）：**
(a) 2a 上线后仍出现「提交时校验通过、成交后一侧失败、orphan sweep 无法覆盖」事故——数据源 = RiskAlert「pair leg orphaned」（service.rs:9998-10010）+ chase 被拒日志（:10141）+ flatten 落空（持仓仍在簿、下 tick 重复判）；动作 = 启动方案 3 评审。
(b) E-VA「but only at SUBMISSION」边界（service.rs:9787-9789 doc-comment）被证明无法靠提交前校验收窄——论证记录进 RFC；动作同上。
(c) 新 venue 接入要求 pair 生命周期级原子性——接入评审记录；动作同上。

注：监控 1-3 的锚点行号以验证时点为准，详见 `docs/arb-arch-acceptance.md`（含与 Issue 正文的锚点勘误，如 chase 归因键是 `pair-chase:` 前缀而非 `:escalated` 后缀）。

---
