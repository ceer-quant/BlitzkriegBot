//! Diagnostics/stats readouts and view assembly (#780): pure move
//! from `service/engine_cycle`.

use super::*;

impl Core {
    /// Per-strategy accounting (P-1.1): open exposure (live) plus session order
    /// counters and realized PnL per registered strategy.
    ///
    /// PR H: one row per strategy NAME (a strategy assigned to two accounts is
    /// one strategy — the old per-scope fan-out produced duplicate rows the
    /// panel read as separate strategies). The row aggregates across scopes and
    /// carries `accounts`: the account ids that hold a live scope of this
    /// strategy, so the panel can slice to the active account. The
    /// `enabled` flag reads "any scope enabled" — a name is either globally
    /// togglable or per-scope from the assignment write, and both arrive here.
    ///
    /// 口径（issue 552 审计）：`enabled` 是 master-power 位（默认 scope 的
    /// 全局开关），**不是** (账户,策略) 复合键的真值 —— 复合真值在
    /// `accounts`（该策略有 live scope 的账户 = 启用集点名了它）。面板已
    /// 不直接消费该位（策略页按钮按账户 feature-detect）；Overview 的
    /// 「尚无启用策略」提示走同一复合口径。
    pub fn strategy_stats(&self) -> Vec<serde_json::Value> {
        let enabled = self.enabled_strategy_names();
        // The row set is the engine's registered names UNION every name that
        // already has accounting slices. A strategy whose scope was torn down
        // (or a test core without an engine) still owns its closed-trade
        // history — dropping the row would make realized PnL vanish from the
        // page mid-session.
        let mut names = self.strategy_names();
        for k in self.strategy_accounting.keys() {
            if let Some(name) = k.split('\u{1}').next()
                && !names.iter().any(|n| n == name)
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                // Fold the (strategy, account) map across every scope of this
                // strategy: the row's counters are the strategy's TOTALS, and
                // `accounts` names the slices a per-account view can pick.
                // The set is the LIVE SCOPES (engine registry, the same
                // read `strategy.list` rows carry) UNION the accounting keys
                // (a torn-down scope's closed-trade history must keep its
                // attribution even after its scope is gone).
                let mut acc = StrategyAccounting::default();
                let prefix = format!("{name}\u{1}");
                let mut accounts: Vec<String> = self
                    .engine
                    .as_ref()
                    .map(|e| e.strategy_scopes(&name))
                    .unwrap_or_default();
                for k in self.strategy_accounting.keys() {
                    if let Some(a) = k.strip_prefix(&prefix)
                        && !accounts.iter().any(|v| v == a)
                    {
                        accounts.push(a.to_string());
                    }
                }
                accounts.sort();
                accounts.dedup();
                for a in &accounts {
                    if let Some(one) = self.strategy_accounting.get(&strategy_acc_key(&name, a)) {
                        acc.placed += one.placed;
                        acc.rejected += one.rejected;
                        acc.limit_rejected += one.limit_rejected;
                        acc.closed_trades += one.closed_trades;
                        acc.wins += one.wins;
                        acc.losses += one.losses;
                        acc.fees_usd += one.fees_usd;
                        acc.net_pnl_usd += one.net_pnl_usd;
                        for (cause, n) in &one.rejection_causes {
                            *acc.rejection_causes.entry(cause.clone()).or_default() += n;
                        }
                    }
                }
                let opens: Vec<&crate::position::OpenPosition> = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.strategy == name)
                    .collect();
                let open_notional: Decimal = opens.iter().map(|p| p.cost_usd).sum();
                // Source: the live scope's provenance when one exists, else the
                // sticky package_sources entry (a torn-down scope's row keeps
                // naming where the package lives — the strategies page shows
                // every INSTALLED strategy, enabled or not).
                let source = self
                    .engine
                    .as_ref()
                    .map(|e| {
                        e.strategy_source(&name).map(|s| s.to_string()).or_else(|| {
                            e.scope_sources()
                                .into_iter()
                                .find(|(n, _)| n == &name)
                                .map(|(_, s)| s)
                        })
                    })
                    .unwrap_or_default()
                    .unwrap_or_default();
                // E2-a: the sizing actually in force for this strategy (its own
                // override clamped by the globals, else the globals) plus the
                // configured caps, so the report shows both quota and occupancy.
                let effective = self
                    .engine
                    .as_ref()
                    .map(|e| e.effective_sizing(&name))
                    .unwrap_or(crate::engine::EffectiveSizing {
                        size_usd: self.config.size_usd,
                        min_shares: self.config.min_shares,
                        max_shares: self.config.max_shares,
                        size_pct: self.config.size_pct,
                        strategy_scoped: false,
                        size_weight: None,
                    });
                let limit = self.config.strategy_limits.get(&name);
                // E2-b: what this strategy DECLARED it does not need, next to how
                // often a gate actually stopped it and how often an exemption let
                // a candidate through — so an opt-out is never invisible beside an
                // un-exempted strategy's rejections.
                let declared = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.strategy_gate_exemptions(&name))
                    .unwrap_or_default();
                let tally = self
                    .engine
                    .as_ref()
                    .map(|e| e.gate_tally(&name))
                    .unwrap_or_default();
                let mut row = serde_json::json!({
                    "name": name,
                    "enabled": enabled.contains(&name),
                    "source": source,
                    // PR H: the account scopes holding this strategy (sorted;
                    // empty = registered but assigned to no account).
                    "accounts": accounts,
                    "openPositions": opens.len(),
                    "openNotionalUsd": dec_json(open_notional),
                    // ── E2-a quota + effective sizing ──
                    "maxOpenPositions": limit.and_then(|l| l.max_open_positions),
                    "maxOpenNotionalUsd": limit
                        .and_then(|l| l.max_open_notional_usd)
                        .map(dec_json),
                    "sizingSource": if effective.strategy_scoped { "strategy" } else { "global" },
                    "effectiveSizeUsd": dec_json(effective.size_usd),
                    "effectiveMinShares": dec_json(effective.min_shares),
                    "effectiveMaxShares": dec_json(effective.max_shares),
                    "sizeWeight": effective.size_weight.map(dec_json),
                    // #202: the equity-relative budget in force for this leg and
                    // what it is worth on the CURRENT balance, so "how much will
                    // this account commit per entry?" is a number on the panel
                    // rather than a reading of the flags.
                    "effectiveSizePct": dec_json(effective.size_pct),
                    "entryBudgetUsd": dec_json(
                        self.engine
                            .as_ref()
                            .map(|e| e.entry_budget_usd(&name))
                            .unwrap_or(effective.size_usd),
                    ),
                    // ── E2-b declared gate exemptions + per-strategy gate counts ──
                    "gateExemptions": declared.gates(),
                    // D-31: the declared `time_left_sec` floor, or null when the
                    // strategy left it to the kernel. Null is meaningful here —
                    // it says "this opt-out still stops at the global window".
                    "gateExemptionTimingFloorSec": declared.timing_min_time_left_sec,
                    "blockedTiming": tally.blocked_timing,
                    "blockedMomentum": tally.blocked_momentum,
                    "gateExemptedTiming": tally.exempted_timing,
                    "gateExemptedMomentum": tally.exempted_momentum,
                    "ordersPlaced": acc.placed,
                    "ordersRejected": acc.rejected,
                    "limitRejected": acc.limit_rejected,
                    // E9-c: cause → count; only counted buckets appear.
                    "rejectionCauses": if acc.rejection_causes.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(acc.rejection_causes)
                    },
                    "closedTrades": acc.closed_trades,
                    "wins": acc.wins,
                    "losses": acc.losses,
                    "feesUsd": dec_json(acc.fees_usd),
                    "netPnlUsd": dec_json(acc.net_pnl_usd),
                });
                // 套利合计字段（契约）：仅「声明被受理」的行携带 —— RFC-645
                // A-1（决议②③）：套利面的裁判从名字比较改为声明驱动。受理
                // 谓词 = 该策略声明 consumes_pair_book_stats() 且声明
                // atomic_batch() 且配对簿里有它 owner_strategy 名下至少一条
                // 在册（非 Closed）条目，三者缺一 = 视为未声明（不附加
                // arb* 字段），并在声明在场却未被受理时 warn 一条（说明缺
                // 哪条腿）。M2 过渡期：名字恰好等于 ARB_STRATEGY 的行仍按
                // 旧名字分支受理（保留一个发布周期），命中时打一次性的
                // deprecation 告警；M3 删名字分支。双腿平仓各记在该策略的
                // (strategy, 各腿账户) 键下（`on_position_closed`），上面的
                // fold 已经是该策略的全口径合计，原样再报一遍专名字段，前端
                // 不必知道键形状；`arbActivePairs` 读配对簿的在册（非
                // Closed）配对数。未受理行不带这些键。
                let arb_accepted = self
                    .engine
                    .as_ref()
                    .map(|e| e.pair_book_stats_accepted(&name, &self.pair_book))
                    .unwrap_or(false);
                // M2 deprecation bridge: the NAME branch (an exact
                // ARB_STRATEGY match) stays authoritative for one release
                // cycle. A row that rides the name instead of an accepted
                // declaration gets a one-shot (per process) deprecation warn;
                // a row whose declaration IS accepted rides the new path
                // silently.
                let name_bridge = name == ARB_STRATEGY && !arb_accepted;
                if name_bridge {
                    arb_name_branch_warn_once(&name);
                }
                if arb_accepted || name_bridge {
                    if let Some(reason) = self
                        .engine
                        .as_ref()
                        .and_then(|e| e.pair_book_stats_rejection_reason(&name, &self.pair_book))
                    {
                        tracing::warn!(
                            strategy = name.as_str(),
                            reason,
                            "consumes_pair_book_stats declared but NOT accepted — the arb* face \
                             rides the deprecated name branch (ARB_STRATEGY match) or is omitted \
                             entirely; a pair consumer must also declare atomic_batch and hold at \
                             least one live PairBook entry"
                        );
                    }
                    row["arbNetPnlUsd"] = dec_json(acc.net_pnl_usd);
                    row["arbWins"] = serde_json::json!(acc.wins);
                    row["arbLosses"] = serde_json::json!(acc.losses);
                    row["arbClosedTrades"] = serde_json::json!(acc.closed_trades);
                    row["arbActivePairs"] =
                        serde_json::json!(self.pair_book.active_pairs_count(&name));
                }
                row
            })
            .collect()
    }

    /// PR H: per-STRATEGY accounting for ONE account — the same counters as
    /// `strategy_stats`, folded over a single account's scopes only. The
    /// strategy page slices with this so a strategy that trades on two
    /// accounts does not print account B's PnL on account A's page.
    pub fn strategy_stats_for_account(&self, account: &str) -> Vec<serde_json::Value> {
        let mut rows = self.strategy_stats();
        // Per-scope counters of just this account, folded back over the row's
        // name — the shape stays identical to `strategy_stats` (the panel's
        // table code renders one shape), only the counter values differ.
        for row in &mut rows {
            let Some(name) = row.get("name").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            // The arb totals the GLOBAL row just carried (the arb unit's whole
            // fold across its leg accounts); read before any slice overwrites
            // the row, and re-attached below so every account's page shows the
            // arb unit's combined take — the legs' own slices stay per-account
            // in the plain counters. RFC-645 A-1: which rows carry the arb*
            // keys was decided by the acceptance predicate in `strategy_stats`
            // (declaration accepted, or the M2 name bridge) — the global row's
            // `arbNetPnlUsd` presence IS the verdict, so this read follows it
            // verbatim and the slice re-attachment cannot drift from the row
            // that produced it.
            let arb_totals = if row.get("arbNetPnlUsd").is_some() {
                Some((
                    row.get("arbNetPnlUsd").cloned().unwrap_or_default(),
                    row.get("arbWins").cloned().unwrap_or_default(),
                    row.get("arbLosses").cloned().unwrap_or_default(),
                    row.get("arbClosedTrades").cloned().unwrap_or_default(),
                    row.get("arbActivePairs").cloned().unwrap_or_default(),
                ))
            } else {
                None
            };
            let one = self
                .strategy_accounting
                .get(&strategy_acc_key(&name, account))
                .cloned()
                .unwrap_or_default();
            row["ordersPlaced"] = serde_json::json!(one.placed);
            row["ordersRejected"] = serde_json::json!(one.rejected);
            row["limitRejected"] = serde_json::json!(one.limit_rejected);
            row["rejectionCauses"] = if one.rejection_causes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(one.rejection_causes)
            };
            row["closedTrades"] = serde_json::json!(one.closed_trades);
            row["wins"] = serde_json::json!(one.wins);
            row["losses"] = serde_json::json!(one.losses);
            row["feesUsd"] = dec_json(one.fees_usd);
            row["netPnlUsd"] = dec_json(one.net_pnl_usd);
            // Gate counts follow the account too: the engine folds them per
            // (strategy, account), so this account's page never prints a
            // sibling account's blocked/exempted numbers.
            let tally = self
                .engine
                .as_ref()
                .map(|e| e.gate_tally_for_account(&name, account))
                .unwrap_or_default();
            row["blockedTiming"] = serde_json::json!(tally.blocked_timing);
            row["blockedMomentum"] = serde_json::json!(tally.blocked_momentum);
            row["gateExemptedTiming"] = serde_json::json!(tally.exempted_timing);
            row["gateExemptedMomentum"] = serde_json::json!(tally.exempted_momentum);
            // Open exposure: THIS account's slice of the strategy.
            let opens: Vec<&crate::position::OpenPosition> = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.strategy == name && p.account_id.as_str() == account)
                .collect();
            row["openPositions"] = serde_json::json!(opens.len());
            row["openNotionalUsd"] = dec_json(opens.iter().map(|p| p.cost_usd).sum());
            // The arb unit's combined take rides EVERY account's page
            // (契约：无论面板选哪个账户，套利行显示套利单元合计收益).
            if let Some((net, wins, losses, closed, pairs)) = arb_totals {
                row["arbNetPnlUsd"] = net;
                row["arbWins"] = wins;
                row["arbLosses"] = losses;
                row["arbClosedTrades"] = closed;
                row["arbActivePairs"] = pairs;
            }
        }
        rows
    }

    /// Diagnostic snapshot: feed counters + engine trend/confirmed state.
    pub fn engine_stats(&self) -> serde_json::Value {
        self.engine_stats_at(now_ms())
    }

    /// E29 (§12.2): `kline.history` body — the aggregator's closed tail plus
    /// the still-growing bar as the last element. A core without an engine
    /// answers the empty list: the documented envelope for "no bars exist",
    /// exactly what the Wave-0 placeholder arm used to answer.
    pub fn kline_history(
        &self,
        symbol: &str,
        interval: crate::kline::KlineInterval,
        limit: usize,
    ) -> Vec<crate::kline::Kline> {
        self.engine
            .as_ref()
            .map(|e| e.kline_history(symbol, interval, limit))
            .unwrap_or_default()
    }

    /// E29: aggregation counters for the stats surface (§10.3: drops must be
    /// visible). Zeros when no engine is attached.
    pub fn kline_stats(&self) -> Option<crate::kline::AggregatorStats> {
        self.engine.as_ref().map(|e| e.kline_stats())
    }

    /// The same snapshot as of an explicit instant. The token lists are sorted:
    /// their source is a `HashSet`, whose iteration order is randomised per
    /// process, and a report that shuffles between runs cannot be diffed.
    pub fn engine_stats_at(&self, as_of_ms: i64) -> serde_json::Value {
        // P0 #670: fd 配额使用率——EMFILE 自旋（accept 刷日志写满磁盘）的
        // 前夜观测。只在本次 stats 求值时算一次（面板轮询频率），不在数据/
        // 订单路径上。open/usagePct 在 macOS 上为 null（无 procfs），
        // critical 随之恒 false。
        let probe_fd = crate::probe::fd_usage();
        let probe_fd_json = serde_json::json!({
            "soft": probe_fd.as_ref().map(|u| u.soft),
            "hard": probe_fd.as_ref().map(|u| u.hard),
            "open": probe_fd.as_ref().and_then(|u| u.open),
            "usagePct": probe_fd.as_ref().and_then(|u| u.usage_pct()),
            "critical": probe_fd.as_ref().is_some_and(|u| u.critical()),
        });
        let mut confirmed = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_tokens().into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        confirmed.sort();
        let mut confirmed_detail = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_diagnostics(as_of_ms))
            .unwrap_or_default();
        confirmed_detail.sort_by(|a, b| a["token"].as_str().cmp(&b["token"].as_str()));
        let blocked = self
            .engine
            .as_ref()
            .map(|e| {
                // Per-strategy attribution (E2-b / #27): the totals stay global,
                // and `byStrategy` names whose candidates each block belonged to.
                let mut by_strategy = serde_json::Map::new();
                for name in self.strategy_names() {
                    let t = e.gate_tally(&name);
                    if t.blocked_timing == 0 && t.blocked_momentum == 0 {
                        continue;
                    }
                    by_strategy.insert(
                        name,
                        serde_json::json!({
                            "timing": t.blocked_timing,
                            "momentum": t.blocked_momentum,
                        }),
                    );
                }
                let declared: Vec<serde_json::Value> = e
                    .declared_gate_exemptions()
                    .into_iter()
                    .map(|(name, x)| {
                        // D-31: name the remaining-time floor beside the gates, so
                        // "which opt-outs reach into the closing window" is
                        // answerable from the stats snapshot alone.
                        let mut v = serde_json::json!({ "strategy": name, "gates": x.gates() });
                        if let Some(n) = x.timing_min_time_left_sec {
                            v["timingFloorSec"] = serde_json::json!(n);
                        }
                        v
                    })
                    .collect();
                serde_json::json!({
                    "timing": e.blocked_timing_count(),
                    "momentum": e.blocked_momentum_count(),
                    "byStrategy": by_strategy,
                    "declaredExemptions": declared,
                })
            })
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "books": self.stats.books,
            "tops": self.stats.tops,
            "spots": self.stats.spots,
            "rounds": self.stats.rounds,
            // #377 (治 #2): archive verdict events replayed into the core —
            // settlements that came from the stream instead of the dry ladder.
            "resolutions": self.stats.resolutions,
            // #388: wallet-level cash rows credited from the stream — the
            // count and exact USD total per kind, so a report reconciles the
            // credit against the ledger it built.
            "cashflowRebates": self.stats.cashflow_rebates,
            "cashflowRewards": self.stats.cashflow_rewards,
            "cashflowRebatesUsd": dec_json(self.stats.cashflow_rebates_usd),
            "cashflowRewardsUsd": dec_json(self.stats.cashflow_rewards_usd),
            "evaluations": self.stats.evaluations,
            "signals": self.stats.signals,
            "placeRejected": self.stats.place_rejected,
            "strategyLimitRejected": self.stats.strategy_limit_rejected,
            "venueRejected": self.stats.venue_rejected,
            "illegalPriceRejected": self.stats.illegal_price_rejected,
            // #180: the ONE panel-visible error slot — structured (code + when +
            // what), written by every internal refusal path, and read by the
            // panel's error banner. `code` is the same `CoreErrorCode`
            // vocabulary a rejected RPC carries in `data.coreCode`, so a client
            // that knows how to branch on one knows how to branch on the other.
            "lastError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "code": e.code,
                    "message": e.message,
                }),
                None => serde_json::Value::Null,
            },
            // Legacy spelling of the same record, kept because the shipped panel
            // (ui/webapp/webui/src/pages/Overview.vue) reads this key for its
            // "last trading error" banner. Same record, one writer, one
            // timestamp — never a second, staler copy. The message is rendered
            // exactly as this key always rendered it (`<code>: <message>`), so a
            // consumer that parsed the old string keeps working; `lastError`
            // above is the structured form to migrate to.
            "lastVenueError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "message": e.legacy_message(),
                }),
                None => serde_json::Value::Null,
            },
            // E31-b: how close the reconcile sweep is to freezing trading. The
            // counter is the missing half of the last error — the error says a
            // sweep failed, the counter says how many in a row, and the threshold
            // says when the freeze lands. Read-only: nothing here sets either
            // value, and the sweep's own success still resets the streak
            // (`Core::reconcile`). Exposed because the transition itself is only
            // reachable from a market plugin, so this is the only way an outside
            // observer (panel, gate) can see the kernel's own threshold.
            "reconcile": {
                "consecutiveSweepFailures": self.consecutive_sweep_failures,
                "freezeThreshold": SWEEP_FAILURE_FREEZE,
            },
            "selfCheck": match &self.last_self_check {
                Some(r) => serde_json::json!({
                    "ok": r.ok,
                    "tsMs": r.ts_ms,
                    "items": r.items.iter().map(|i| serde_json::json!({
                        "name": i.name, "ok": i.ok, "detail": i.detail,
                    })).collect::<Vec<_>>(),
                }),
                None => serde_json::Value::Null,
            },
            "tradingFrozen": if self.risk.is_killed() {
                serde_json::json!({
                    "active": true,
                    "reason": self.risk.kill_reason().unwrap_or("kill switch active"),
                })
            } else {
                serde_json::json!({ "active": false })
            },
            // The account-scoped kill book (account.freeze / auto-freeze on
            // the venue-rejection streak): account id → why. Empty object when
            // nothing is frozen — a client distinguishes "global freeze"
            // (tradingFrozen.active) from "these accounts only" without
            // parsing reasons.
            "accountFreezes": self.account_freezes,
            // In-kernel accounting audit (issue #189): the latest verdict, so the
            // panel shows what the kernel itself concluded about the books.
            "accountingAudit": self.accounting_audit_view(),
            // #173: the day's realized-loss budget as the panel reads it —
            // visible even when healthy, so "is the breaker armed, and against
            // what cap?" is answerable without grepping the startup log.
            // The daily budget is PER-ACCOUNT (2026-10-25 verdict): the card
            // mirrors the DISPLAY account's day (the panel follows the account
            // everywhere since #592), and carries every account's day beside it
            // so no book's freeze is invisible.
            "dailyLoss": {
                "account": self.accounts.display_id(),
                "dayIndex": self.positions.daily_state_for(self.accounts.display_id()).day_index,
                "realizedPnlUsd": self
                    .positions
                    .daily_pnl_for(self.accounts.display_id()),
                "limitUsd": self
                    .positions
                    .effective_daily_loss_limit_for_public(self.accounts.display_id()),
                "openingEquityUsd": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .opening_equity_usd,
                // P1 #235: WHICH book that base belongs to ("dry"/"live", null
                // when the day predates the field). The cap is only as
                // meaningful as the account it is a share of, and the panel
                // used to show a self-consistent pair of numbers from two
                // different books.
                "equityBasis": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .equity_basis,
                "tripped": self
                    .positions
                    .daily_loss_tripped_for(self.accounts.display_id()),
                "trippedAtMs": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .tripped_at_ms,
                "anyTripped": self
                    .positions
                    .daily_states()
                    .values()
                    .any(|s| s.tripped),
                "accounts": self
                    .positions
                    .daily_states()
                    .iter()
                    .map(|(id, s)| serde_json::json!({
                        "account": id.as_str(),
                        "realizedPnlUsd": s.realized_pnl_usd,
                        "tripped": s.tripped,
                        "dayIndex": s.day_index,
                    }))
                    .collect::<Vec<_>>(),
                // #177: protective stops the wick guard withheld — "should have
                // triggered" is a number on the panel, not a silent no-op.
                "suppressedStops": self.positions.suppressed_stop_count(),
            },
            // #205: how stale a book may be before the engine stops pricing off
            // it — the value that actually gates entries, read off the engine
            // that applies it (a backtest can configure its own). "How long
            // after the feed goes quiet does the bot stop trading?" is a runtime
            // question and must be answerable from the panel.
            "orderbookFreshness": self.orderbook_freshness_view(),
            // P0 #202: what ONE order can commit on THIS account. The ceiling is
            // read off the kernel's own knobs — no ticket can hold more than
            // `max_shares` (every leg is clamped to it) or pay over `max_price`
            // a share, and when the equity-relative cap is armed the
            // `balance × k` bound applies on top. `scripts/risk-sizing-check.mjs`
            // asserts exactly the last line of this block on a live dry core.
            "sizing": self.sizing_view(),
            // #190: the pending-close exit-reason table — how many reasons are
            // held against the bound, and what the two clean-up paths removed.
            // `orphaned` must read 0: an entry whose position is gone is the
            // ghost reason the table used to accumulate.
            "exitReasons": self.exit_reasons_view(),
            // Settlement & redemption (issue #175): settled-but-unredeemed claims
            // are money the chain still owes, and this is where the panel sees
            // them — the same outlet as everything else, no new event type.
            "settlement": self.settlement_view(as_of_ms),
            "blocked": blocked,
            "confirmed": confirmed,
            "confirmedDetail": confirmed_detail,
            "strategies": self.strategy_stats(),
            "archive": self
                .event_archive
                .as_ref()
                .map(|a| serde_json::to_value(a.status()).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            // E29 (§12.2/§10.3): the K-line aggregator's counters — drops MUST
            // be visible (§10.3), and this is where an outside observer sees
            // them. `droppedOutOfOrder` counts discarded late ticks,
            // `noDataBars` counts silently skipped empty buckets. Null when no
            // engine is attached (same posture as `archive`).
            "kline": self
                .kline_stats()
                .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            // 环境差异排查探针（任务二 1-3）：时钟偏差 / 数据流断流 / spot
            // 失效的观测面。字段名即契约（tests/env_probes.rs 钉死）：
            // * ntp: 上次 SNTP 测量（offsetMs/skewRed/失败计数）
            // * tickHealth: 最近 10s 汇总（窗口 tick 数/零 tick token/最长
            //   静默/断流 warn 计数）
            // * spot: 停更监控（最近更新时刻/停更时长/warn 计数）
            // * fd: P0 #670 的 fd 配额使用率（函数开头算好的 probe_fd_json）。
            "probe": serde_json::json!({
                "ntp": {
                    "synced": self.probe_health.probe_ntp.last_offset_ms.is_some(),
                    "offsetMs": self.probe_health.probe_ntp.last_offset_ms,
                    "skewRed": self.probe_health.probe_ntp.skew_red(),
                    "failures": self.probe_health.probe_ntp.failures,
                    "lastCheckMs": self.probe_health.probe_ntp.last_check_ms,
                },
                "tickHealth": {
                    // 进行中的窗口计数（实时）。汇总不立刻清零：值被
                    // capture 留档，下一个非汇总 tick 才重新计数——
                    // 汇总瞬间读 stats 必须还能看到刚结束的窗口。
                    "windowTicks": self.probe_health.probe_ticks.window_ticks,
                    "zeroTickTokens": self.probe_health.probe_last_zero,
                    "maxSilenceMs": self.probe_health.probe_last_max_silence_ms,
                    "streamBreakWarnings": self.probe_health.probe_ticks.stream_break_warnings,
                    "lastSummaryMs": self.probe_health.probe_ticks.last_summary_ms,
                },
                "spot": {
                    "lastSeenMs": self.probe_health.probe_spot.last_seen_ms,
                    "staleMs": self.probe_health.probe_last_spot_stale_ms,
                    "staleWarnings": self.probe_health.probe_spot.stale_warnings,
                },
                "fd": probe_fd_json,
            }),
        })
    }

    /// P0 #202 — "how much can any ONE order commit on this account, right
    /// now?", answered from the kernel's own knobs so the panel (and the sizing
    /// gate) never re-derive it from the source:
    ///
    /// * `shareBandUsd` = `max_shares × max_price`: the widest ticket the
    ///   engine's own clamp allows, valued at the risk gate's own price ceiling.
    ///   This is the bound that existed before #202, and on the live 4.8 USDC
    ///   account it is 10.00 USD — 208% of the book.
    /// * `sizeBudgetUsd` (#202, when `size_pct > 0`) = `equity × pct%`.
    /// * `equityCapUsd` (#202, when the relative cap is armed) = `equity × k%`,
    ///   the bound the risk gate REJECTS over.
    /// * `worstCaseOrderUsd` = the tightest of the three. Closing intents are
    ///   exempt from the equity cap by design (`closes_exposure`), and a close
    ///   is bounded by the position it reduces — never by these knobs — so the
    ///   number describes NEW exposure, which is what "max possible loss on one
    ///   order" means for a binary-market BUY (the whole notional can go to 0).
    pub(super) fn sizing_view(&self) -> serde_json::Value {
        // E28: the sizing view describes the ACTIVE account's book (the panel
        // is a per-session view of the session's account, §9.5).
        let equity = self.accounts.active_ledger().balance();
        let globals = self.engine.as_ref().map(|e| e.global_sizing()).unwrap_or(
            crate::engine::EffectiveSizing {
                size_usd: self.config.size_usd,
                min_shares: self.config.min_shares,
                max_shares: self.config.max_shares,
                size_pct: self.config.size_pct,
                strategy_scoped: false,
                size_weight: None,
            },
        );
        let risk = self.risk.config();
        let share_band = globals.max_shares.max(Decimal::ZERO) * risk.max_price.max(Decimal::ZERO);
        let able = equity > Decimal::ZERO;
        let size_budget = (globals.size_pct > Decimal::ZERO && able)
            .then(|| equity * globals.size_pct / Decimal::ONE_HUNDRED);
        let equity_cap = (risk.max_order_notional_pct > Decimal::ZERO && able)
            .then(|| equity * risk.max_order_notional_pct / Decimal::ONE_HUNDRED);
        let mut worst = share_band;
        if let Some(b) = size_budget {
            worst = worst.min(b);
        }
        if let Some(c) = equity_cap {
            worst = worst.min(c);
        }
        serde_json::json!({
            "equityUsd": dec_json(equity),
            "minShares": dec_json(globals.min_shares),
            "maxShares": dec_json(globals.max_shares),
            "maxPrice": dec_json(risk.max_price),
            "shareBandUsd": dec_json(share_band),
            "sizePct": dec_json(globals.size_pct),
            "sizeBudgetUsd": size_budget.map(dec_json),
            "maxOrderNotionalUsd": dec_json(risk.max_order_notional),
            "maxOrderNotionalPct": dec_json(risk.max_order_notional_pct),
            "equityCapUsd": equity_cap.map(dec_json),
            "worstCaseOrderUsd": dec_json(worst),
            "worstCasePctOfEquity": if able {
                dec_json(worst / equity * Decimal::ONE_HUNDRED)
            } else {
                serde_json::Value::Null
            },
            // The escape hatch, stated where the bounds are: a close/reduce is
            // never judged by the equity cap, so a bound that would trap a
            // position cannot be read off this block either (#174/#202).
            "closesExemptFromEquityCap": true,
            "sizePctSkippedSignals": self
                .engine
                .as_ref()
                .map(|e| e.size_pct_skip_count())
                .unwrap_or(0),
        })
    }

    /// The latest accounting audit as the panel reads it. `null` until the first
    /// audit runs (the audit interval has not elapsed, or it is disabled).
    pub fn accounting_audit_view(&self) -> serde_json::Value {
        let mut v = match self.audit.report.as_ref() {
            Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            None => serde_json::json!({ "ok": true, "note": "audit not run yet" }),
        };
        v["halted"] = serde_json::json!(self.audit.halted);
        v["haltReason"] = serde_json::json!(self.audit.halt_reason);
        v["runs"] = serde_json::json!(self.audit.runs);
        v["failures"] = serde_json::json!(self.audit.failures);
        v["anchored"] = serde_json::json!(self.audit.anchor.is_some());
        // Settled-but-unredeemed payout: money owed to us, part of the identity's
        // `total` but not of `balance` until the redeem transaction confirms
        // (issue #175). `mergeReceivableUsd` splits out #461's burned-pair
        // collateral waiting on its merge receipt — both are inside
        // `receivableUsd` (the identity telescopes over all receivables).
        // The view mirrors the AUDITED identity: the ACTIVE account's own
        // receivable only (a sibling account's claim is a different wallet).
        let active_account = self.accounts.active_id().as_str().to_string();
        v["receivableUsd"] = dec_json(self.settlement.receivable_usd_for(&active_account));
        v["mergeReceivableUsd"] =
            dec_json(self.settlement.merge_receivable_usd_for(&active_account));
        v["summary"] = serde_json::json!(
            self.audit
                .report
                .as_ref()
                .map(|r| r.summary())
                .unwrap_or_default()
        );
        v
    }

    /// Snapshot the current round for the UI.
    pub fn round_view(&self) -> Option<RoundView> {
        let engine = self.engine.as_ref()?;
        let now = now_ms();
        let r = engine.scanner().round_state(now);
        let can_trade = engine.scanner().can_trade(now).is_ok();
        // One price card per ASSET: a round may carry several markets for the
        // same asset (strike ladders), and the panel keys the cards by asset —
        // duplicates there are rendered as repeated cards with colliding keys.
        let mut market_prices: Vec<MarketPriceView> = Vec::new();
        for m in &r.markets {
            if market_prices.iter().any(|p| p.asset == m.asset) {
                continue;
            }
            let up = engine
                .book_snapshot(&m.up_token_id)
                .map(|b| b.mid_price)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(m.up_price);
            let down = engine
                .book_snapshot(&m.down_token_id)
                .map(|b| b.mid_price)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(m.down_price);
            market_prices.push(MarketPriceView {
                asset: m.asset.clone(),
                up,
                down,
            });
        }
        Some(RoundView {
            slot: r.slot,
            age_sec: r.age_sec,
            time_left_sec: r.time_left_sec,
            markets: r.markets.len(),
            can_trade,
            market_prices,
        })
    }

    /// Read-only depth view for the UI (`engine.books`, E8-c 盘口深度).
    ///
    /// Reads `Core.books` — the mirror that every ingest path updates (Node
    /// `books.snapshot` bridge and the Rust-native feed alike), and the same
    /// store the exit path values open positions from — so the view can never
    /// contradict what trading actually saw. Levels and metrics come from
    /// `OrderbookSnapshot::from_levels`, the same builder the strategy layer
    /// consumes. Purely observational: no locks beyond the core's own, no side
    /// effects. An absent round or an unknown token simply yields nothing.
    ///
    /// #560 的 slot 过滤 + (venue, asset) 唯一化在这里同样适用：快照装配层
    /// 只修了 `engine.round` 面，`engine.books` 是第二个逐市场平铺的数据源
    /// ——slot 边界附近相邻两轮（当前+1 与当前+2）并存于 scanner，盘口
    /// 深度卡每资产出现两行、选择器按 (venue,asset) 撞重复 key。与
    /// `engine.round` 同判据：到期 slot = 当前 slot + 1（正在交易的一轮），
    /// round_slot == 0（老语料 serde default）照旧放行；再先到先得唯一化。
    /// 判定钟由调用方传入（`round_state` / `current_slot` 同风格）：窗口边
    /// 界附近「喂入轮恰好是视图当下认定的 trading slot」是真实链路里插件
    /// 5s 轮询自带的性质，测试要精确钉住这个关系就不能赌宿主墙钟不翻格。
    ///
    /// E2（多族读面）：过滤谓词是 scanner 的共用纯函数
    /// `is_live_round_row` —— 每行按自己的 (venue, duration) 族钟判定
    /// 「到期 slot = 该族当前 slot + 1」，`engine.round` 行情行（ipc/server）
    /// 与本视图同谓词。老语料（round_slot == 0）无条件放行，单族部署下
    /// 族钟 ≡ 全局网格（偏移 0、行声明 ≡ cfg），判定与升级前的
    /// `current_slot(now) + 1` 逐位一致。
    pub fn books_view(&self, max_levels: usize, now: i64) -> Vec<AssetBooksView> {
        let Some(engine) = self.engine.as_ref() else {
            return Vec::new();
        };
        let scanner = engine.scanner();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        // RFC-661 M3: natural rows only. The dual-key table holds every
        // venue's OWN (venue, token) rows (M1), so the round-state sweep IS
        // the whole display.
        let rows: Vec<AssetBooksView> = scanner
            .round_state(now)
            .markets
            .iter()
            .filter(|m| scanner.is_live_round_row(m, now))
            .filter(|m| seen.insert((m.venue.clone(), m.asset.clone())))
            .map(|m| AssetBooksView {
                venue: m.venue.clone(),
                asset: m.asset.clone(),
                up_token_id: m.up_token_id.clone(),
                down_token_id: m.down_token_id.clone(),
                up: Self::book_side_view(&self.books, &m.up_token_id, max_levels),
                down: Self::book_side_view(&self.books, &m.down_token_id, max_levels),
            })
            .collect();
        rows
    }

    pub(super) fn book_side_view(
        books: &HashMap<TokenId, Book>,
        token_id: &str,
        max_levels: usize,
    ) -> BookSideView {
        let Some(b) = books.get(token_id) else {
            return BookSideView::empty();
        };
        if b.bids.is_empty() && b.asks.is_empty() {
            return BookSideView::empty();
        }
        let s = OrderbookSnapshot::from_levels(
            token_id.to_string(),
            b.bids.clone(),
            b.asks.clone(),
            now_ms(),
        );
        let level = |(p, q): &(Decimal, Decimal)| BookLevelView {
            price: *p,
            size: *q,
        };
        BookSideView {
            bids: s.bids.iter().take(max_levels).map(level).collect(),
            asks: s.asks.iter().take(max_levels).map(level).collect(),
            best_bid: Some(s.best_bid).filter(|p| *p > Decimal::ZERO),
            best_ask: Some(s.best_ask).filter(|p| *p < Decimal::ONE),
            mid_price: Some(s.mid_price).filter(|p| *p > Decimal::ZERO),
            obi: Some(s.obi),
            spread: Some(s.spread),
            spread_pct: Some(s.spread_pct),
        }
    }

    /// Open positions enriched for the UI (unrealised PnL uses the current price
    /// we track; the book tick updates it on every check).
    pub fn position_views(&self, now_ms: i64) -> Vec<crate::schema::PositionView> {
        use crate::exit_policy::pnl_pct;
        self.positions
            .open_positions()
            .iter()
            .map(|p| crate::schema::PositionView {
                id: p.id.clone(),
                asset: p.asset.clone(),
                direction: p.direction.as_str().to_string(),
                strategy: p.strategy.clone(),
                token_id: p.token_id.clone(),
                // E28 (§9.2): the account the position settles into — a field
                // read, and the row the panel filters per account.
                account_id: p.account_id.as_str().to_string(),
                // #427: the Venue column — a field read off the position.
                venue: p.venue.clone(),
                entry_price: p.entry_price,
                current_price: p.current_price,
                shares: p.shares,
                unrealized_pct: pnl_pct(p.current_price, p.entry_price),
                high_pnl_pct: p.high_pnl_pct(),
                was_maker_entry: p.was_maker_entry,
                entered_at_ms: p.entered_at_ms,
                expires_at_ms: p.expires_at_ms,
                remaining_sec: (p.expires_at_ms - now_ms) / 1000,
                // Slot matching for the panel's expiry judgment — same
                // grid coordinate the orders tab already carries.
                round_slot: p.round_slot,
                // The position's OWN family duration (three-state fallback
                // resolved exactly as the force-close gate resolves it), so
                // the panel's per-row slot arithmetic uses the family clock
                // the kernel judges with — not the page's identity-venue
                // duration, which is a different family under multi-family
                // deployment (issue 733).
                round_duration_sec: p
                    .effective_round_duration_sec(None, self.config.round_duration_sec),
                // Cash actually moved on this position, so an external monitor
                // can check the ledger identity without waiting for a flat book.
                // `entry_cost_usd` is the TOTAL entry notional (what left the
                // ledger); `cost_usd` is only the basis still held, so it is the
                // former the identity must sum.
                entry_cost_usd: p.flows.entry_cost_usd,
                cost_usd: p.cost_usd,
                entry_fee_usd: p.flows.entry_fee_usd,
                proceeds_usd: p.flows.proceeds_usd,
                exit_fee_usd: p.flows.exit_fee_usd,
            })
            .collect()
    }
}
