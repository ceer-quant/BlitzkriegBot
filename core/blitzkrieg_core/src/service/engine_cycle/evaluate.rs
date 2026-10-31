//! The Lua evaluation cycle and its replay fast path (#780): pure
//! move from `service/engine_cycle`.

use super::*;

impl Core {
    /// Run one evaluation cycle: engine produces entry orders, core places them.
    /// Entries go through the usual risk/capacity gates; rejections are skipped.
    /// A configured per-strategy cap (P-1.1) is checked before the order layer.
    /// Replay fast path: whether THIS cycle must run a real Lua evaluation —
    /// an engine input arrived since the last one (`fast_eval_wake`), a
    /// round-timing boundary the gates read is due (`fast_next_timing_ms`),
    /// or the fallback window elapsed. Between those, the same inputs
    /// reproduce the same output, so the call is skipped.
    pub(super) fn fast_eval_due(&self, now_ms: i64) -> bool {
        self.fast_path.fast_eval_wake
            || now_ms >= self.fast_path.fast_next_timing_ms
            || now_ms - self.fast_path.fast_last_eval_ms >= FAST_EVAL_FALLBACK_MS
    }

    pub fn engine_evaluate(&mut self, now_ms: i64) -> usize {
        if self.engine.is_none() {
            return 0;
        }
        self.stats.evaluations += 1;
        // Replay fast path: a quiet tick skips the Lua call entirely. Between
        // engine inputs (book/top/spot/rollover — the dirty wake) the only
        // strategy inputs that move are the round clock's integer-second
        // fields, and their gate verdicts cannot flip inside the open
        // interval the boundary wake (`fast_next_timing_ms`) covers; the
        // fallback bounds anything the boundary does not model to 2 s.
        // The evaluations COUNTER still advances every cycle: the sparse-
        // cadence contract (`sparse_stream_evaluates_every_due_cycle`) pins
        // the scheduled cycle count, not the wall cost of one cycle.
        // #390: the nofast replay runs the same gates through
        // `replay_scheduling` — the strategy surface it observes (signals,
        // fills, trades) is identical, and the eval-call health counter is
        // host diagnostic, not replay semantics.
        if self.replay_sched() && !self.shadow_evolution.is_enabled() && !self.fast_eval_due(now_ms)
        {
            // Issue #390 (replay nofast): a QUIET cycle. The base walk ran a
            // full Lua collection here and re-submitted its suggestions into
            // the kernel-side gates; this cycle re-runs the STANDING
            // suggestions of the last real evaluation through the SAME
            // kernel-side pipeline instead — the gate/counter/log/placement
            // stream is reproduced cycle for cycle without the Lua call
            // (see `engine_replay_cycle`). A stable-phase cache answers with
            // orders; an unstable one (a phase transition the last real
            // evaluation introduced — a latch fired, an emission consumed)
            // refuses, and THIS cycle runs a real evaluation: the base's
            // next collection would collect the post-transition output.
            // The wake flags stay armed either way: a quiet cycle consumes
            // neither the input wake nor the timing boundary — both belong
            // to the next REAL evaluation.
            if let Some(orders) = self.engine_replay_cycle(now_ms) {
                return orders;
            }
        }
        if self.replay_sched() {
            // A real evaluation consumes the wake; the timing boundary is
            // recomputed after the engine borrow ends (below).
            self.fast_path.fast_eval_wake = false;
            self.fast_path.fast_last_eval_ms = now_ms;
            self.fast_path.fast_next_timing_ms = i64::MAX;
        }
        // Shadow Evolution: evaluate FIRST so any applied hot-swap is in force
        // for this very cycle's order decision (next-tick semantics, no restart).
        self.shadow_evolution_evaluate(now_ms);
        // E13: the 72h DEEP round rides the same cadence, so a long-running
        // process re-anchors its variant sets (compound mutants) without an
        // operator. The check itself is one comparison per cycle.
        self.shadow_evolution_maybe_cycle(now_ms);
        // #390: read before the engine borrow opens (the re-arm below uses it
        // while `engine` is still mutably borrowed).
        let replay_sched = self.replay_sched();
        // The position snapshot strategies see via `bk.holdings()` — built
        // before the engine borrow opens (same once-per-cycle cadence as the
        // equity push below).
        let held_view = self.held_position_view();
        // RFC 568 阶段一：本周期每策略的配对簿视图快照（owner 过滤在 Core
        // 侧做完 —— 实例主账户 + arb 腿账户白名单，与 holdings 视图拓宽同
        // 形）。也在引擎借用打开前构建；策略层经 `bk.arb.pairs()` 只读
        // （最终一致，附注 2）。
        let pair_views: Vec<crate::pair::PairView> = self
            .engine
            .as_ref()
            .map(|e| {
                e.hosted()
                    .iter()
                    .filter(|s| s.enabled)
                    .map(|s| {
                        let mut accounts: HashSet<String> = HashSet::new();
                        accounts.insert(s.account_id.clone());
                        if let Some(ls) = s.strategy.arb_leg_accounts() {
                            accounts.extend(ls.iter().cloned());
                        }
                        (s.strategy.name().to_string(), accounts)
                    })
                    .collect::<Vec<(String, HashSet<String>)>>()
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(name, accounts)| self.pair_views_for(&name, &accounts))
            .collect();
        let engine = self.engine.as_mut().expect("engine present");
        // #202: the equity-relative sizing is a percentage of the account, and
        // the account is the ledger — pushed here, once per cycle, so a ticket
        // can never be sized against a balance the process no longer has.
        // E28: the legacy scalar mirrors the ACTIVE account's book; the
        // per-scope pushes below carry every configured account's own book,
        // trading switch, and venue binding, so each scope sizes/gates
        // against ITS account. (§9.4: the kernel drives strategies; per-
        // account routing happens when each order passes through the gates.)
        engine.set_equity_usd(self.accounts.active_ledger().balance());
        for a in self.accounts.account_list() {
            let id = a.id.as_str();
            let equity = self
                .accounts
                .ledger_of(&a.id)
                .map(|l| l.balance())
                .unwrap_or_default();
            engine.set_account_equity(id, equity);
            engine.set_account_trading(id, a.trading_enabled);
            engine.set_account_venue(id, &a.venue);
            engine.set_account_frozen(id, self.account_freezes.contains_key(id));
        }
        engine.set_positions_view(held_view);
        // 视图落引擎的 per-cycle 快照（evaluate 的 ctx 构造从这取）。
        engine.set_pair_views_snapshot(pair_views);
        let orders = engine.evaluate(now_ms);
        // Replay fast path: recompute when the round-timing verdict can next
        // flip — quiet ticks skip the Lua call until then (or the fallback
        // beat, or a new engine input). #390: nofast replay re-arms the same
        // boundary. (`replay_sched` was read before the engine borrow opened.)
        if replay_sched {
            self.fast_path.fast_next_timing_ms =
                engine.next_timing_boundary_ms(now_ms).unwrap_or(i64::MAX);
        }
        // E30 (§6.3): drain the sandbox poison alerts now; each is raised as
        // ONE RISK_ALERT after the engine borrow ends below — the message
        // names the strategy, the sandbox itself refuses every later call
        // from it, and the kernel keeps running (a poisoned strategy is
        // quarantined, not a crash).
        // #363: the round timing this cycle was judged against, copied out
        // while the engine borrow is alive so the policy facts below do not
        // extend it (the engine reference must end before `self.emit`).
        let policy_time_left_sec = engine.last_time_left_sec();
        let poison_alerts = engine.drain_poison_alerts();
        for (name, alert) in &poison_alerts {
            tracing::error!(strategy = %name, %alert, "lua strategy poisoned; all further calls refused");
        }
        // Close intents produced by strategies this cycle join the SAME exit
        // submission path as policy exits (handled in run_exit_checks).
        self.strategy_exits.extend(engine.drain_strategy_exits());
        engine.tally_blocked();
        // E2-b: an honoured gate exemption is always logged (and counted), so an
        // opt-out is auditable rather than a silent hole in the entry gates.
        for rec in engine.take_exemptions() {
            tracing::info!(target: "strategy", "{}", rec.audit_line());
        }
        // RFC 568 阶段一：A≠B 拒绝回执与配对声明都在引擎借用内先收下来
        // （回执 → APPLICATION 事件、声明 → precheck 过闸后 declare，都在
        // 借用结束后 —— 与 poison_alerts 的先收后发同一模式）。
        let pair_refusals = engine.take_pair_refusals();
        let pair_declarations = engine.take_pair_declarations();
        for (name, alert) in poison_alerts {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!("lua strategy '{name}' poisoned: {alert}"),
            });
        }
        self.stats.signals += orders.len() as u64;
        let mut tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        // ── RFC 568 阶段一（补钉 5 的消费侧）：A≠B 校验的拒绝回执 →
        // APPLICATION 错误事件（点名字段 pair.key）。订单路径照旧 —— 校验
        // 拒的是「声明」，不是腿（裸腿由 arb_precheck 的整组语义与内核去重
        // 兜底）；声明被拒 ⇒ 该腿不进 PairBook。
        for reason in pair_refusals {
            self.emit(Event::Error {
                error: CoreError::new(CoreErrorCode::InvalidParams, reason),
            });
        }
        // ── E-VA: the atomic-batch pre-reservation, judged BEFORE the first
        // leg's gate runs — the whole-cycle dimensions (credentials/scope,
        // RFC #541 hook B) plus the per-account pot dimension. Strategies
        // that declared `atomic_batch` (and sit in `virtual_limits`) reserve
        // their WHOLE batch against the virtual pot: an over-cap batch is
        // cancelled wholesale — zero legs placed, zero real reservations
        // taken — with the refusal bucketed through the existing
        // StrategyLimit accounting. Everything else (every shipped strategy,
        // every undeclared batch) finds no governed scope and skips
        // untouched.
        self.arb_precheck_filter(&mut tokens);
        // ── RFC 568 阶段一（补钉 3 的落点）：声明受理在 arb_precheck 过闸
        // 之后 —— 预检拒整批 ⇒ 不入账（凭证维度由预检管，声明不重复校验）；
        // 过闸 ⇒ 逐腿 declare 进 PairBook。存活判定直接用 precheck 后的
        // 幸存单集（strategy, 落账账户, token）三元组唯一对应一条腿：scope
        // 整组丢、策略丢、去重吞掉的声明都不入账（配对簿只记将要发生的事
        // 实的配对骨架）；经济身份 (asset, slot) 在 pair_declare_leg 里从腿
        // token 经 scanner 派生。
        let survived: HashSet<(String, String, String)> = tokens
            .iter()
            .map(|(_, req)| {
                (
                    req.strategy.clone(),
                    req.account_id.to_string(),
                    req.token_id.clone(),
                )
            })
            .collect();
        for d in pair_declarations {
            if !survived.contains(&(
                d.owner_strategy.clone(),
                d.leg_account.clone(),
                d.token_id.clone(),
            )) {
                continue;
            }
            self.pair_declare_leg(&d, now_ms);
        }
        let mut placed = 0;
        // ── #363: the execution policy's account-level verdict, judged BEFORE
        // arbitration. Two decisions ride here:
        //
        //   1. `Skip` (a rule hit or the equity floor) — the intent never
        //      reaches `place()`, so it never reaches the RiskGate either;
        //      the refusal is bucketed `policy.skip` and audited with the
        //      rule's own reason. This is the ONLY place a policy verdict
        //      removes an order: everything downstream (RiskGate, breakers,
        //      capacity, reserve, OME) runs untouched — "规则不能绕过
        //      RiskEngine" holds because the policy can only REMOVE an
        //      intent, never relax a gate or resize past the risk cap.
        //   2. `Place { budget_usd }` — the budget is an UPPER BOUND: the
        //      request's own share band (min_shares/max_shares, engine
        //      sizing) still decides the ticket, and the policy shrinks it
        //      when the shares cost more than the budget allows. With the
        //      shipped defaults (ratio 0.10, floor 1, ceiling 50) the bound
        //      sits at or above every ticket the share band can produce, so
        //      no order changes size — exit-economics parity by construction.
        let policy_verdicts: HashMap<
            String,
            Result<crate::execution_policy::PolicyOutput, crate::execution_policy::PolicyRefusal>,
        > = if self.execution_policy.is_some() {
            // The open_positions fact counts LIVE positions only — an expired
            // round's stuck position is settlement-dead (same rule as
            // PositionManager::can_open) and must not trip the shipped
            // `open_positions >= N` backstop.
            let open = self.positions.open_positions();
            let open_for = |acct: &str| {
                open.iter()
                    .filter(|p| p.account_id.as_str() == acct && p.expires_at_ms > now_ms)
                    .count() as u32
            };
            tokens
                .iter()
                .map(|(_, req)| {
                    let facts = crate::execution_policy::PolicyInput {
                        available_balance: self
                            .accounts
                            .ledger_of(&req.account_id)
                            .map(|l| l.balance())
                            .unwrap_or_default(),
                        total_equity: self
                            .accounts
                            .ledger_of(&req.account_id)
                            .map(|l| l.balance())
                            .unwrap_or_default(),
                        open_positions: open_for(req.account_id.as_str()),
                        current_price: req.price,
                        time_left_sec: policy_time_left_sec,
                        symbol: req.asset.clone(),
                        recent_pnl_1h: self
                            .positions
                            .recent_pnl_1h_for(req.account_id.as_str(), now_ms),
                        consecutive_losses: self.breaker.consecutive_losses(&req.strategy),
                    };
                    let verdict = crate::execution_policy::evaluate(
                        self.execution_policy.as_ref().expect("checked above"),
                        req.account_id.as_str(),
                        &facts,
                    );
                    (req.account_id.to_string() + ":" + &req.token_id, verdict)
                })
                .collect()
        } else {
            HashMap::new()
        };
        // ── v0.3 Wave 0 (#329): the E25 arbitration delegation seam ──────────
        // E25 inserts its one-line per-intent delegation at the head of the
        // loop below: `process_intent` (arbitration/) runs BEFORE the E16
        // portfolio cap, so every strategy intent is arbitrated exactly once
        // and the SAME Request that cleared the gates is the one handed to
        // `self.place(...)`. Wave 0 lands the seam only — a comment, not a
        // call: `arbitration/` does not exist yet and this PR is
        // behavior-free by charter (deviation disclosed in the PR body).
        for (token, mut req) in tokens {
            let name = req.strategy.clone();
            // ── #406: the zero/negative-price fail-closed guard — the FIRST
            // placement gate, before the policy arm. A strategy that names a
            // price <= 0 on an entry is a malformed intent (a prediction
            // market has no legal quote at or below zero), and every price
            // the ticket touches downstream DIVIDES by it: the policy's
            // budget shrink (`budget_usd / req.price`, service.rs Place arm)
            // is the earliest such division, and a zero price PANICKED the
            // process there (rust_decimal Division by zero) before the
            // arbitration Gate 1 price-band check — which would have refused
            // this intent — ever ran. Fail closed instead: the entry is
            // refused with its own counter and cause bucket, the process
            // keeps running, and the audit trail names the strategy.
            // Closing intents are exempt: an exit prices off the book and
            // position state, never off the strategy's entry quote, and a
            // held position must always be closable.
            if req.side == crate::model::Side::Buy && req.price <= Decimal::ZERO {
                tracing::error!(target: "strategy",
                    "entry rejected: strategy={name} cause=illegal.price token={} price={} — \
                     non-positive entry price is refused fail-closed (#406): it would divide \
                     by zero in sizing/policy/physics",
                    req.token_id, req.price);
                self.stats.place_rejected += 1;
                self.stats.illegal_price_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                *acc.rejection_causes
                    .entry("illegal.price".to_string())
                    .or_default() += 1;
                continue;
            }
            // ── #363: account-level cooldown gate. A rule that returned
            // `Cooldown` last cycle parked the ACCOUNT here for N seconds:
            // every entry this account names is refused until the clock runs
            // out (the clock is the caller's — the rule states a duration,
            // the kernel owns the time). Expired entries are dropped on
            // sight, and nothing about exits is slowed: only entries.
            if let Some(&until) = self.policy_cooldowns.get(req.account_id.as_str()) {
                if until > now_ms {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.cooldown account={} \
                         {}s left", req.account_id, (until - now_ms + 999) / 1000);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                self.policy_cooldowns.remove(req.account_id.as_str());
            }
            // ── #363: the policy verdict for THIS intent (judged above, before
            // any gate). A Skip refuses here — the intent never reaches the
            // RiskGate, which is the only way a policy may touch an order: it
            // removes, never relaxes. A Place caps the ticket size.
            match policy_verdicts
                .get(&format!("{}:{}", req.account_id, req.token_id))
                .map(|v| v.as_ref())
            {
                Some(Ok(crate::execution_policy::PolicyOutput::Skip { reason })) => {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.skip reason={reason}");
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Cooldown { seconds })) => {
                    let until = now_ms + seconds * 1000;
                    self.policy_cooldowns
                        .insert(req.account_id.to_string(), until);
                    tracing::info!(target: "strategy",
                        "policy cooldown: account={} {}s (entries resume at {})",
                        req.account_id, seconds, until);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Place { budget_usd, .. })) => {
                    // The budget is an UPPER BOUND on the ticket, not a
                    // replacement for the sizing path: the engine's share band
                    // already decided `req.size`, and the policy only shrinks it
                    // when the shares cost more than the account's budget allows.
                    // With the shipped defaults the bound never bites (see the
                    // comment at the verdict map), so no existing order changes.
                    // #779: a tiny positive price passes the illegal-price
                    // guard but overflows `Decimal::div` (a panic that kills
                    // the trading task). Refuse the entry instead — the
                    // `shares < ONE` arm below emits no order.
                    let shares = match budget_to_shares(*budget_usd, req.price) {
                        Some(shares) => shares,
                        None => {
                            tracing::warn!(target: "strategy",
                                "policy budget sizing overflow: strategy={name} account={} \
                                 internal_key={} budget={budget_usd} price={} — entry refused",
                                req.account_id, req.internal_key, req.price);
                            Decimal::ZERO
                        }
                    };
                    if shares >= Decimal::ONE && req.size > shares {
                        tracing::info!(target: "strategy",
                            "policy budget shrink: strategy={} account={} {} -> {shares} shares \
                             (budget {budget_usd} USD)",
                            name, req.account_id, req.size);
                        req.size = shares;
                    }
                    // A budget that cannot buy one whole share emits nothing:
                    // a 0-size order would only turn the refusal into a venue
                    // rejection (the #202 lesson, applied to the policy path).
                    if shares < Decimal::ONE {
                        tracing::info!(target: "strategy",
                            "entry rejected: strategy={name} cause=policy.skip reason=\
                             budget {budget_usd} USD cannot buy one share at {}",
                            req.price);
                        self.stats.place_rejected += 1;
                        let acc = self
                            .strategy_accounting
                            .entry(strategy_acc_key(&name, req.account_id.as_str()))
                            .or_default();
                        acc.rejected += 1;
                        *acc.rejection_causes
                            .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                            .or_default() += 1;
                        continue;
                    }
                }
                Some(Err(refusal)) => {
                    // "account_id 缺失 → 拒绝下单": the policy could not judge
                    // this intent at all. Refuse loudly, never fall through to
                    // defaults.
                    tracing::warn!(target: "strategy",
                        "entry refused: strategy={name} account={} policy refusal: {}",
                        req.account_id, refusal.reason);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                None => {}
            }
            // ── E25 (#331): arbitration BEFORE the E16 portfolio cap — this is
            // the seam Wave 0 planted. Every strategy intent is arbitrated
            // exactly once through the four gates; a Rejected suggestion never
            // reaches place()/OME, an Approved one is handed to the SAME path
            // as before. The gates reuse the existing implementations
            // (OrderIntent::validate / RiskGate / breakers / Ledger::reserve /
            // effective_stop_pct), so no threshold has two truths (§3.3).
            // E28 (§9.2): the intent is arbitrated in the account its own
            // request names — the pipeline's reserve lands in THAT book, not
            // the active one. The posture is judged before the request moves
            // into the intent.
            let account_id = req.account_id.clone();
            let is_close = crate::risk::is_close_intent(&req.internal_key);
            let account_entry_block = self
                .accounts
                .permits_order(&account_id, is_close)
                .err()
                .map(|e| e.message);
            let intent = crate::arbitration::StrategyIntent::from_request(req);
            let outcome = {
                let round_tokens = self
                    .engine
                    .as_ref()
                    .map(|e| e.round_token_ids())
                    .unwrap_or_default();
                let time_left_sec = self
                    .engine
                    .as_ref()
                    .map(|e| e.last_time_left_sec())
                    .unwrap_or(0);
                // E26 (§4.2) + #461: the systemic aggregates are read BEFORE
                // the account borrow — resting BUY notional joins the
                // exposure (cash already promised).
                let systemic_facts = self.systemic_facts(&intent.request.asset, now_ms);
                // The PHYSICS projection needs the strategy's own doctrine:
                // a hold-to-settlement declaration widens the bound stop
                // (the wide containment width, not the 12% ladder).
                let strategy_holds_to_settlement = self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.strategy_holds_to_settlement(&intent.request.strategy));
                // E28 (§9.3): the pipeline's reserve happens in THIS account's
                // book. An unknown account is refused — no implicit empty book,
                // ever (reverse acceptance C) — with the same Gate-2 accounting
                // as an arbitration rejection.
                match self.accounts.get_mut(&account_id) {
                    Err(e) => crate::arbitration::Outcome {
                        decision: crate::arbitration::Decision::Rejected {
                            reason: crate::arbitration::RejectReason::AccountLimit,
                            gate: crate::arbitration::GateId::Risk,
                            detail: e.message,
                        },
                        gates: Vec::new(),
                        latency_us: 0,
                    },
                    Ok(ledger) => {
                        // E28: the #202 equity is a percentage of THIS
                        // account's live balance, same rule as the submission
                        // path.
                        let equity = ledger.balance();
                        let mut ctx = crate::arbitration::IntentCtx {
                            round_tokens: &round_tokens,
                            risk: &self.risk,
                            breaker: &self.breaker,
                            ledger,
                            exit_cfg: &self.config.positions.exit,
                            time_left_sec,
                            now_ms,
                            equity,
                            account_entry_block,
                            systemic_facts,
                            strategy_holds_to_settlement,
                        };
                        crate::arbitration::process_intent(&intent, &mut ctx)
                    }
                }
            };
            self.record_intent_decision(&intent, &outcome, now_ms);
            if let crate::arbitration::Decision::Rejected {
                reason,
                gate,
                detail,
                ..
            } = &outcome.decision
            {
                self.stats.place_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, intent.request.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                // Bucket through the EXISTING classifier over the gate's own
                // message, so the attribution vocabulary is the one place()
                // would have written — the rejection happened earlier, the
                // story an operator reads does not change (E9-c buckets).
                let cause = classify_rejection(&crate::model::CoreError::new(
                    crate::arbitration::reason_code(*reason),
                    detail.clone(),
                ));
                *acc.rejection_causes.entry(cause).or_default() += 1;
                tracing::info!(target: "strategy",
                    "entry rejected: strategy={name} cause=arbitration gate={gate:?} reason={reason:?} detail={detail}");
                continue;
            }
            let mut req = intent.request;
            // E26 (§4.2): a systemic shrink rides the decision as MODIFIED —
            // the audit names the bound that bit; here the reduced size is
            // what gets placed. The pipeline never rewrote the request (the
            // suggestion's provenance stays exactly what the engine emitted),
            // so the shrink is applied at the one point that spends money.
            if let crate::arbitration::Decision::Modified {
                modification: crate::arbitration::Modification::SizeReduced { approved, .. },
                ..
            } = &outcome.decision
            {
                tracing::info!(target: "strategy",
                    "entry modified: strategy={name} systemic shrinks {} -> {approved} shares",
                    req.size);
                req.size = *approved;
            }
            // E16/#98 portfolio-level exposure cap (0 = off): the account's
            // total open commitment across ALL strategies is bounded too —
            // per-strategy caps partition it, but their sum needs its own
            // ceiling. The check ADDS a constraint; the RiskGate hard bounds
            // (kill switch, price band, per-order cap) are untouched.
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit, now_ms)
            {
                // E9-c: the reason existed before but was discarded — bucket
                // it so operator tooling can answer "why did this strategy
                // stop placing?".
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            // E-VA: the batch pre-check passed — HOLD this leg's virtual
            // quota before it spends anything real. A later per-order gate
            // may still refuse the leg: the Err arm below releases its keyed
            // reservation, and a placed leg keeps its hold until the order
            // state machine's terminal transition releases it.
            {
                let account_id = req.account_id.to_string();
                let strategy = req.strategy.clone();
                self.virtual_batch_commit(
                    &account_id,
                    &strategy,
                    &[(req.internal_key.clone(), req.price * req.size)],
                );
            }
            let placed_account = req.account_id.clone();
            let virtual_leg = if self
                .virtual_accounts
                .governed(placed_account.as_str(), &name)
            {
                Some((req.internal_key.clone(), req.price * req.size, req.side))
            } else {
                None
            };
            // RFC 568 阶段一：place 前回填 internal_key —— 声明腿与 OME 订单
            // 的事实钥匙挂钩（成交/死亡事件经它寻址回这条腿）。只对 BUY 入腿
            // 有意义（SELL 是退出，不入配对簿）；未声明的 token 是空操作。
            if req.side == Side::Buy {
                self.pair_bind_key(&name, placed_account.as_str(), &token, &req.internal_key);
            }
            // `entry_maker` doctrine: a pure-maker package's leg carries the
            // OME's never-escalate clock (`maker_timeout_ms < 0` — see
            // `escalation_delay_ms`); an unfilled quote rests to the round's
            // own end-of-life. Every other strategy keeps the configured
            // maker→taker window, byte-identically.
            let maker_timeout = if self
                .engine
                .as_ref()
                .is_some_and(|e| e.strategy_entry_maker(&name))
            {
                -1
            } else {
                self.config.entry_maker_timeout_ms
            };
            match self.place(req, maker_timeout, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default()
                        .placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_scoped_order_placed(placed_account.as_str(), &token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    // E-VA: a refused leg never rests — release the keyed
                    // virtual hold its batch took, so the pot reflects only
                    // commitments that actually exist.
                    if let Some((key, _notional, side)) = &virtual_leg
                        && *side == Side::Buy
                    {
                        self.virtual_accounts.release(
                            placed_account.as_str(),
                            &name,
                            key,
                            *_notional,
                        );
                    }
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    // E9-c: attach the live reason text verbatim to the receipt
                    // log line so per-strategy accounting and logs tell the
                    // same story.
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
                }
            }
        }
        placed
    }

    /// Issue #390 (replay nofast): one QUIET evaluation cycle — the base
    /// walk's every-tick Lua collection replaced by a re-submission of the
    /// standing candidates through the SAME kernel-side pipeline
    /// (`engine.replay_candidates` → the shared submit half). Everything the
    /// base cycle produced from the re-collected suggestions — the
    /// `signals`/`place_rejected` counters, the per-strategy accounting and
    /// rejection buckets, the entry-rejected log lines, the gate verdicts and
    /// their placement instants — is reproduced cycle for cycle without the
    /// Lua call; the counters are the schedule's, not the wall cost's
    /// (mirroring `replay_credit_cycles`). Deliberately NOT touched: the eval
    /// wake flags (a quiet cycle consumes neither the input wake nor the
    /// timing boundary — both belong to the next REAL evaluation, whose
    /// Lua collection refreshes the staged set), the shadow-evolution cadence
    /// (a jumping driver is refused while it is enabled), and the eval-call
    /// health counters (host diagnostic, exempt host fields).
    /// Issue #390 (replay nofast): one QUIET evaluation cycle — the base
    /// walk's every-tick Lua collection replaced by a re-submission of the
    /// standing candidates through the SAME kernel-side pipeline
    /// (`engine.replay_candidates` → the shared submit half). Everything the
    /// base cycle produced from the re-collected suggestions — the
    /// `signals`/`place_rejected` counters, the per-strategy accounting and
    /// rejection buckets, the entry-rejected log lines, the gate verdicts and
    /// their placement instants — is reproduced cycle for cycle without the
    /// Lua call; the counters are the schedule's, not the wall cost's
    /// (mirroring `replay_credit_cycles`). Returns `None` when the staged
    /// set is not in a stable phase (the caller falls back to a real
    /// evaluation). Deliberately NOT touched: the eval wake flags (a quiet
    /// cycle consumes neither the input wake nor the timing boundary — both
    /// belong to the next REAL evaluation, whose Lua collection refreshes the
    /// staged set), the shadow-evolution cadence (a jumping driver is refused
    /// while it is enabled), and the eval-call health counters (host
    /// diagnostic, exempt host fields).
    pub(super) fn engine_replay_cycle(&mut self, now_ms: i64) -> Option<usize> {
        let held_view = self.held_position_view();
        let staged = self
            .engine
            .as_mut()
            .map(|e| {
                // #202: the same per-cycle equity push the real cycle makes,
                // so a replayed ticket is sized against the balance the
                // process has NOW. The per-scope mirrors ride along (the
                // replay book is single-default — the loop is one iteration,
                // byte-identical outcomes with the pre-scope shape).
                e.set_equity_usd(self.accounts.active_ledger().balance());
                for a in self.accounts.account_list() {
                    let id = a.id.as_str();
                    let equity = self
                        .accounts
                        .ledger_of(&a.id)
                        .map(|l| l.balance())
                        .unwrap_or_default();
                    e.set_account_equity(id, equity);
                    e.set_account_trading(id, a.trading_enabled);
                    e.set_account_venue(id, &a.venue);
                    e.set_account_frozen(id, self.account_freezes.contains_key(id));
                }
                e.set_positions_view(held_view);
                e.replay_candidates(now_ms)
            })
            .unwrap_or_else(|| Some(Vec::new()))?;
        let orders = staged;
        self.stats.signals += orders.len() as u64;
        let mut tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        // ── E-VA: the SAME whole-cycle pre-check the real cycle runs (issue
        // #390's parity rule: the nofast twin reproduces the base cycle's
        // kernel-side pipeline cycle for cycle). Ungoverned scopes
        // short-circuit before touching anything.
        self.arb_precheck_filter(&mut tokens);
        // The physics projection the base cycle judged (E25): refreshed for
        // THIS cycle's round clock by `replay_candidates`.
        let policy_time_left_sec = self
            .engine
            .as_ref()
            .map(|e| e.last_time_left_sec())
            .unwrap_or(0);
        let mut placed = 0;
        for (token, mut req) in tokens {
            let name = req.strategy.clone();
            // ── #363: the policy verdict for THIS intent. A Skip refuses here
            // — the intent never reaches the RiskGate. A Place caps the
            // ticket size. The shipped defaults never bite (see the comment
            // at the real cycle's verdict map).
            let policy = self.execution_policy.clone();
            let policy_verdict = policy.map(|policy| {
                // open_positions counts LIVE positions only (expired rounds
                // are settlement-dead; see engine_evaluate's verdict map).
                let open = self.positions.open_positions();
                let open_for = |acct: &str| {
                    open.iter()
                        .filter(|p| p.account_id.as_str() == acct && p.expires_at_ms > now_ms)
                        .count() as u32
                };
                let facts = crate::execution_policy::PolicyInput {
                    available_balance: self
                        .accounts
                        .ledger_of(&req.account_id)
                        .map(|l| l.balance())
                        .unwrap_or_default(),
                    total_equity: self
                        .accounts
                        .ledger_of(&req.account_id)
                        .map(|l| l.balance())
                        .unwrap_or_default(),
                    open_positions: open_for(req.account_id.as_str()),
                    current_price: req.price,
                    time_left_sec: policy_time_left_sec,
                    symbol: req.asset.clone(),
                    recent_pnl_1h: self
                        .positions
                        .recent_pnl_1h_for(req.account_id.as_str(), now_ms),
                    consecutive_losses: self.breaker.consecutive_losses(&req.strategy),
                };
                crate::execution_policy::evaluate(&policy, req.account_id.as_str(), &facts)
            });
            match policy_verdict {
                Some(Ok(crate::execution_policy::PolicyOutput::Skip { reason })) => {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.skip reason={reason}");
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Cooldown { seconds })) => {
                    let until = now_ms + seconds * 1000;
                    self.policy_cooldowns
                        .insert(req.account_id.to_string(), until);
                    tracing::info!(target: "strategy",
                        "policy cooldown: account={} {}s (entries resume at {})",
                        req.account_id, seconds, until);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Place { budget_usd, .. })) => {
                    // #779: a tiny positive price passes the illegal-price
                    // guard but overflows `Decimal::div` (a panic that kills
                    // the trading task). Refuse the entry instead — the
                    // `shares < ONE` arm below emits no order.
                    let shares = match budget_to_shares(budget_usd, req.price) {
                        Some(shares) => shares,
                        None => {
                            tracing::warn!(target: "strategy",
                                "policy budget sizing overflow: strategy={name} account={} \
                                 internal_key={} budget={budget_usd} price={} — entry refused",
                                req.account_id, req.internal_key, req.price);
                            Decimal::ZERO
                        }
                    };
                    if shares >= Decimal::ONE && req.size > shares {
                        tracing::info!(target: "strategy",
                            "policy budget shrink: strategy={} account={} {} -> {shares} shares \
                             (budget {budget_usd} USD)",
                            name, req.account_id, req.size);
                        req.size = shares;
                    }
                    if shares < Decimal::ONE {
                        tracing::info!(target: "strategy",
                            "entry rejected: strategy={name} cause=policy.skip reason=\
                             budget {budget_usd} USD cannot buy one share at {}",
                            req.price);
                        self.stats.place_rejected += 1;
                        let acc = self
                            .strategy_accounting
                            .entry(strategy_acc_key(&name, req.account_id.as_str()))
                            .or_default();
                        acc.rejected += 1;
                        *acc.rejection_causes
                            .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                            .or_default() += 1;
                        continue;
                    }
                }
                Some(Err(refusal)) => {
                    tracing::warn!(target: "strategy",
                        "entry refused: strategy={name} account={} policy refusal: {}",
                        req.account_id, refusal.reason);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                None => {}
            }
            // ── E25 (#331): the arbitration seam, identical to the real
            // cycle's (the gates reuse the existing implementations, so no
            // threshold has two truths).
            let account_id = req.account_id.clone();
            let is_close = crate::risk::is_close_intent(&req.internal_key);
            let account_entry_block = self
                .accounts
                .permits_order(&account_id, is_close)
                .err()
                .map(|e| e.message);
            let intent = crate::arbitration::StrategyIntent::from_request(req);
            let outcome = {
                let round_tokens = self
                    .engine
                    .as_ref()
                    .map(|e| e.round_token_ids())
                    .unwrap_or_default();
                // #461: the systemic aggregates are read BEFORE the account
                // borrow — resting BUY notional joins the exposure.
                let systemic_facts = self.systemic_facts(&intent.request.asset, now_ms);
                // Same PHYSICS doctrine read as the real cycle: a
                // hold-to-settlement declaration widens the bound stop.
                let strategy_holds_to_settlement = self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.strategy_holds_to_settlement(&intent.request.strategy));
                // E28 (§9.3): the reserve happens in THIS account's book.
                match self.accounts.get_mut(&account_id) {
                    Err(e) => crate::arbitration::Outcome {
                        decision: crate::arbitration::Decision::Rejected {
                            reason: crate::arbitration::RejectReason::AccountLimit,
                            gate: crate::arbitration::GateId::Risk,
                            detail: e.message,
                        },
                        gates: Vec::new(),
                        latency_us: 0,
                    },
                    Ok(ledger) => {
                        let equity = ledger.balance();
                        let mut ctx = crate::arbitration::IntentCtx {
                            round_tokens: &round_tokens,
                            risk: &self.risk,
                            breaker: &self.breaker,
                            ledger,
                            exit_cfg: &self.config.positions.exit,
                            time_left_sec: policy_time_left_sec,
                            now_ms,
                            equity,
                            account_entry_block,
                            systemic_facts,
                            strategy_holds_to_settlement,
                        };
                        crate::arbitration::process_intent(&intent, &mut ctx)
                    }
                }
            };
            self.record_intent_decision(&intent, &outcome, now_ms);
            if let crate::arbitration::Decision::Rejected {
                reason,
                gate,
                detail,
                ..
            } = &outcome.decision
            {
                self.stats.place_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, intent.request.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                let cause = classify_rejection(&crate::model::CoreError::new(
                    crate::arbitration::reason_code(*reason),
                    detail.clone(),
                ));
                *acc.rejection_causes.entry(cause).or_default() += 1;
                tracing::info!(target: "strategy",
                    "entry rejected: strategy={name} cause=arbitration gate={gate:?} reason={reason:?} detail={detail}");
                continue;
            }
            let mut req = intent.request;
            if let crate::arbitration::Decision::Modified {
                modification: crate::arbitration::Modification::SizeReduced { approved, .. },
                ..
            } = &outcome.decision
            {
                tracing::info!(target: "strategy",
                    "entry modified: strategy={name} systemic shrinks {} -> {approved} shares",
                    req.size);
                req.size = *approved;
            }
            // E16/#98 portfolio-level exposure cap (0 = off).
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit, now_ms)
            {
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            // E-VA: the batch pre-check passed — HOLD this leg's virtual
            // quota before it spends anything real. A later per-order gate
            // may still refuse the leg: the Err arm below releases its keyed
            // reservation, and a placed leg keeps its hold until the order
            // state machine's terminal transition releases it.
            {
                let account_id = req.account_id.to_string();
                let strategy = req.strategy.clone();
                self.virtual_batch_commit(
                    &account_id,
                    &strategy,
                    &[(req.internal_key.clone(), req.price * req.size)],
                );
            }
            let placed_account = req.account_id.clone();
            let virtual_leg = if self
                .virtual_accounts
                .governed(placed_account.as_str(), &name)
            {
                Some((req.internal_key.clone(), req.price * req.size, req.side))
            } else {
                None
            };
            // RFC 568 阶段一：place 前回填 internal_key —— 声明腿与 OME 订单
            // 的事实钥匙挂钩（成交/死亡事件经它寻址回这条腿）。只对 BUY 入腿
            // 有意义（SELL 是退出，不入配对簿）；未声明的 token 是空操作。
            if req.side == Side::Buy {
                self.pair_bind_key(&name, placed_account.as_str(), &token, &req.internal_key);
            }
            // `entry_maker` doctrine: a pure-maker package's leg carries the
            // OME's never-escalate clock (`maker_timeout_ms < 0` — see
            // `escalation_delay_ms`); an unfilled quote rests to the round's
            // own end-of-life. Every other strategy keeps the configured
            // maker→taker window, byte-identically.
            let maker_timeout = if self
                .engine
                .as_ref()
                .is_some_and(|e| e.strategy_entry_maker(&name))
            {
                -1
            } else {
                self.config.entry_maker_timeout_ms
            };
            match self.place(req, maker_timeout, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default()
                        .placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_scoped_order_placed(placed_account.as_str(), &token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    // E-VA: a refused leg never rests — release the keyed
                    // virtual hold its batch took, so the pot reflects only
                    // commitments that actually exist.
                    if let Some((key, _notional, side)) = &virtual_leg
                        && *side == Side::Buy
                    {
                        self.virtual_accounts.release(
                            placed_account.as_str(),
                            &name,
                            key,
                            *_notional,
                        );
                    }
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
                }
            }
        }
        Some(placed)
    }
}
