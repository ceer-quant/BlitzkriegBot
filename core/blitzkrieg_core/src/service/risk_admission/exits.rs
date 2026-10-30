use super::*;

impl Core {
    pub fn cancel(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id))?;
        let was_live = order.status.is_live();
        self.release_order_bookkeeping(&order, was_live, "cancel");
        self.ome.mark_terminal(id, OrderStatus::Cancelled, now_ms)?;
        // The venue may still hold the order resting: hand its id to the live
        // bridge for a real cancel. Nothing to cancel when the venue never
        // acknowledged the order (no id — the venue never heard of it).
        if was_live && let Some(vid) = order.venue_order_id {
            let venue = self.execution_venue_of(order.account_id.as_str(), &order.token_id);
            self.queue_venue_cancel(venue, vid);
        }
        self.emit_order(id);
        Ok(())
    }

    /// Release every bookkeeping hold an order took when it was placed.
    ///
    /// The terminal transition has TWO callers — an explicit [`Self::cancel`]
    /// and the reconcile sweep's `Cancelled`/`ForceClosed` repair (#776) — and
    /// they must run the SAME bookkeeping: a reservation released on one path
    /// but not the other outlives its order, and `run_accounting_audit` flags
    /// the stray hold, halts new entries and never lets go. Keeping one
    /// implementation here is the whole point of the extraction.
    ///
    /// `was_live` is the same guard the cancel path spelled inline: only an
    /// order that actually rested may lift the token's repeat suppression (the
    /// hold-to-settlement gap-fill gate) or dirty the replay schedule. Every
    /// release below is itself idempotent (a keyed remove / a `dead` flag set),
    /// so a second sweep over an already-terminal order is a no-op.
    pub(crate) fn release_order_bookkeeping(
        &mut self,
        order: &crate::model::TrackedOrder,
        was_live: bool,
        why: &str,
    ) {
        if order.side == Side::Buy {
            // E28: the reservation is released in the ORDER'S OWN book — a
            // terminal transition must never refund another account's cash.
            if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                l.release(&order.order_id);
            }
            // E-VA: the same precision for the strategy's VIRTUAL pot — the
            // order's whole remaining hold leaves the book.
            self.virtual_release_order(order);
        }
        // RFC 568 阶段一：死亡事实（撤单/对账终态）喂配对簿。
        self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
        if was_live {
            // The token's repeat-suppression ends with the order: a terminal
            // entry leaves no resting order behind, so a later cycle may
            // re-attempt (the strategy's own latch still bounds it to one
            // attempt per book). Only hold-to-settlement strategies release
            // through this gate — see `release_pending_token_for_strategy`.
            if let Some(engine) = self.engine.as_mut() {
                engine.release_pending_token_for_strategy(
                    order.account_id.as_str(),
                    &order.strategy,
                    &order.token_id,
                );
            }
            // Replay fast path: a live order just left the book — the
            // escalation sweep and the audit input changed.
            if self.replay_sched() {
                self.fast_orders_dirty = true;
            }
        }
        tracing::debug!(
            order = order.order_id.as_str(),
            why,
            "terminal order bookkeeping released"
        );
    }

    /// Enqueue one venue cancel (deduped by id, bounded so a pathological loop
    /// cannot grow it without bound). The live bridge drains it every tick.
    /// `venue` labels the entry for the per-venue executor drain (`""` =
    /// untagged — routes to the active plugin's drain, the single-plugin
    /// behaviour).
    pub(crate) fn queue_venue_cancel(&mut self, venue: String, venue_order_id: String) {
        if !self
            .pending_venue_cancels
            .iter()
            .any(|(_, id)| id == &venue_order_id)
        {
            self.pending_venue_cancels.push((venue, venue_order_id));
            if self.pending_venue_cancels.len() > 256 {
                self.pending_venue_cancels.remove(0);
            }
        }
    }

    /// Drain the venue-cancel queue: once. The bridge must forward each id as
    /// a real `DELETE /order` — see [`Self::cancel`].
    pub fn take_pending_venue_cancels(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_venue_cancels)
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    }

    /// The multi-plugin drain: every cancel id whose order's round market
    /// names `venue`, plus the UNTAGGED ids when `venue` is the host's active
    /// plugin (the single-plugin fallback the orders drain uses). Take-once
    /// per entry: a sibling executor never sees an id this drain returned.
    pub fn take_pending_venue_cancels_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
    ) -> Vec<String> {
        let mut taken = Vec::new();
        let mut rest = Vec::new();
        for (v, id) in std::mem::take(&mut self.pending_venue_cancels) {
            if v == venue || (untagged_fallback && v.is_empty()) {
                taken.push(id);
            } else {
                rest.push((v, id));
            }
        }
        self.pending_venue_cancels = rest;
        taken
    }

    /// E31-c: retire the RESTING remainder of a partially filled order while
    /// keeping the already-filled part in the position book. The strategy
    /// decides WHEN; the kernel guarantees the venue-side cancel rides along.
    pub fn cancel_remaining(&mut self, order_id: &str, now_ms: i64) -> CoreResult<()> {
        self.cancel(order_id, now_ms)
    }

    /// E31-c: flatten the position behind an order — sell every share the
    /// fills have accrued so far, at that position's latest valuation.
    pub fn close_filled(&mut self, order_id: &str, now_ms: i64) -> CoreResult<usize> {
        let token = self
            .ome
            .get(order_id)
            .map(|o| o.token_id.clone())
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, order_id.to_string()))?;
        let position = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.token_id == token)
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorCode::UnknownOrder,
                    format!("no open position behind order {order_id}"),
                )
            })?;
        self.flatten(Some(&position), now_ms)
    }

    pub fn cancel_all(&mut self, token_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| token_id.map(|t| o.token_id == t).unwrap_or(true))
            .map(|o| o.order_id.clone())
            .collect();
        for id in &ids {
            self.cancel(id, now_ms)?;
        }
        Ok(ids.len())
    }

    /// Force-close positions: a specific id, or all when id is None. Returns how
    /// many were closed. Exits are SELL orders; in dry mode they cross at the
    /// latest book bid, in live mode the venue bridge submits them.
    pub fn flatten(&mut self, position_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let targets: Vec<FlattenTarget> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| position_id.map(|id| p.id == id).unwrap_or(true))
            .map(|p| {
                (
                    p.id.clone(),
                    p.token_id.clone(),
                    p.condition_id.clone(),
                    p.shares,
                    p.strategy.clone(),
                    p.asset.clone(),
                    p.direction.as_str().to_string(),
                    p.current_price,
                    // E28 (§9.2): the manual close spends from the position's
                    // own account.
                    p.account_id.clone(),
                )
            })
            .collect();
        let mut closed = 0usize;
        for (id, token, condition, shares, strategy, asset, direction, _current, account_id) in
            targets
        {
            // Already have a live sell for this token? skip.
            if self
                .ome
                .live_for(&token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live())
            {
                continue;
            }
            // Price the exit off the CURRENT book, not the stale valuation:
            // `current_price` is a mid ≈ what the shares are worth, not what
            // a resting bid is offering, so an FOK there never crosses live.
            // The walk names the worst bid level that covers the size. With
            // no bids at all nothing can fill — the request falls back to the
            // grid floor and the settle/venue rejects it honestly.
            let price = self
                .marketable_walk(Side::Sell, &token, shares, Decimal::new(1, 2), now_ms)
                .map(|(_, worst)| worst)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(Decimal::new(1, 2));
            self.note_exit_reason(&token, ExitReason::Manual);
            let order = OrderRequest {
                token_id: token,
                condition_id: condition,
                side: Side::Sell,
                mode: FillPolicy::Taker,
                price,
                size: shares,
                internal_key: format!("flatten:{id}"),
                strategy,
                asset,
                direction,
                round_slot: 0,
                // E28 (§9.2): the closing sell spends from the position's own
                // account — a cross-account flatten must never touch another
                // book.
                account_id,
            };
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            } else {
                closed += 1;
            }
        }
        Ok(closed)
    }

    // ── Maintenance: pending-fill retry + maker→taker escalation + exits ────
    pub fn tick(&mut self, now_ms: i64) -> CoreResult<()> {
        // #351: advance in-flight taker legs — the tick path covers rounds
        // where the clock moves without data events (the replay tail keeps
        // reporting legs after the last event). A no-op when no leg waits.
        self.drain_inflight_takers(now_ms);
        // Flush the market-data archive at most once a second, so a reader of the
        // live file stays close behind without paying a write syscall every tick
        // (P-1.3).
        if let Some(a) = self.event_archive.as_mut() {
            a.flush_if_due(now_ms, 1000);
        }
        // Issue #669: the archive's stop (disk floor / lock / io / session cap)
        // is otherwise a single journal warn line — the external operator who
        // reported it found the silent corpus loss FIVE DAYS later with a manual
        // `df`. Fold the live recording state through the last-seen slot and
        // say it OUT LOUD on the `true → false` edge, exactly once: the risk
        // alert carries the stop reason and what is being lost (backtest
        // corpus). Staying stopped is silent; the panel banner reads
        // `stats.archive` continuously. `false → true` cannot happen (a stopped
        // archive never restarts) but resets the slot defensively.
        if let Some(a) = self.event_archive.as_ref() {
            let recording = a.status().recording;
            if self.archive_recording_last && !recording {
                let message = "事件归档停录——回测语料不再记录；请清理磁盘或调整归档上限后重启会话"
                    .to_string();
                eprintln!("core: {message}");
                tracing::error!("{}", message);
                self.note_error(
                    &CoreError::new(CoreErrorCode::Internal, message.clone()),
                    now_ms,
                );
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::Internal,
                    message,
                });
            }
            self.archive_recording_last = recording;
        }

        // E29 (§12.2): the still-growing bars push at most 1/s per
        // `(symbol, interval)` — the "正在长" preview a chart draws translucent.
        // CLOSED bars are pushed by `engine_on_data` the instant they close and
        // are NEVER subject to this throttle (a swallowed close would leave the
        // chart missing that bar forever). The throttle is per bucket on the
        // CORE's clock, shared by every subscribed session.
        // Replay gate (issue #409 P0, 39.5× measured): the pass is skipped
        // wholesale under `replay_sched()`. Its events are live-panel
        // decoration and NOTHING in a replay consumes them — the backtest
        // drain matches every event it cares about by name and drops
        // `KlineUpdate` in `_ => {}`; the `next_replay_wake_ms` probe has
        // never armed a wake for this pass (its only throttle state is the
        // local 1/s fold); and `engine_on_data` pushes CLOSED bars on its
        // own path, untouched by this gate. The per-bucket 1/s fold, the
        // "one bar one push" contract and the close-vs-preview split are all
        // live semantics; dropping them in replay changes no byte of the
        // report — the A/B in docs/perf/backtest-perf-plan.md measured the
        // output bit-identical with the pass off.
        if !self.replay_sched()
            && let Some(engine) = self.engine.as_ref()
        {
            for bar in engine.kline_current_bars() {
                let key = (bar.symbol.clone(), bar.interval);
                let due = self
                    .kline_preview_last_ms
                    .get(&key)
                    .is_none_or(|last| now_ms - *last >= 1_000);
                if due {
                    self.kline_preview_last_ms.insert(key, now_ms);
                    self.emit(Event::KlineUpdate { kline: bar });
                }
            }
        }

        // Retry buffered fills (orders registered since the event arrived).
        let pending = self.ome.drain_pending(now_ms)?;
        for outcome in pending {
            self.report_fill_outcome(outcome, now_ms);
        }

        // Settlement (issue #175): a position past its market's expiry has to
        // become cash, and only the news of the market's resolution can do it.
        // Runs before the audit so the money a settlement moves is accounted in
        // the same round it happens (the ordering is free: `drive_settlement` is
        // a no-op in live mode, where resolutions arrive from the venue).
        // Replay fast path: the whole pass is a pure function of the position
        // set and the two deadline clocks (query re-query, redemption retry) —
        // skipped until the position set changes, the earliest expiry passes,
        // or a deadline arrives. A skipped tick would have dispatched nothing
        // and stamped nothing, so every dispatch keeps its exact stamp.
        // A pending merge claim joins the deadlines: its dispatch clock must
        // keep ticking even when nothing else in the pass moves.
        let settlement_pass = !self.replay_sched()
            || self.fast_positions_dirty
            || now_ms >= self.fast_next_expiry_ms
            || now_ms >= self.settlement.next_query_deadline_ms()
            || now_ms >= self.settlement.next_redemption_ms()
            || now_ms >= self.settlement.next_merge_ms();
        if settlement_pass {
            self.drive_settlement(now_ms);
            // A market we are watching whose verdict never arrives strands the
            // position: say so once per episode (the panel carries
            // `blindSinceMs` continuously, this is the alert). The blind edge
            // only changes when queries/answers move, so riding the same gate
            // delays the alert at most to the next deadline tick.
            self.alert_if_settlement_blind(now_ms);
            // #684: same gate, one step further — a market the venue will
            // STRUCTURALLY never answer (expired rounds drop out of the updown
            // list) cannot be settled by waiting. The sweep only fires when
            // `unanswered_since` names it, so a healthy book pays one
            // hash-set build per settlement pass and nothing else.
            self.run_expiry_fallback_sweep(now_ms);
        }

        // Accounting audit (issue #189): the kernel's own periodic three-way money
        // check, so drift is caught (and blocks new entries) even when nothing
        // external is polling. Cheap: sums over live orders and open positions.
        if self.config.audit_interval_sec > 0 {
            let due = self.audit.last_at_ms == 0
                || now_ms - self.audit.last_at_ms >= self.config.audit_interval_sec * 1000;
            if due {
                self.run_accounting_audit(now_ms);
            }
        }

        // Resume entries when a per-strategy breaker cooldown elapses (KI-10):
        // each strategy resumes independently and is reported by name.
        for strategy in self.breaker.maybe_resume_all(now_ms) {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "breaker cooldown elapsed — resuming new entries for strategy {strategy}"
                ),
            });
        }

        // 探针 10s 汇总（任务二 1-3，受控时钟）：tick 断流 / spot 失效的周期
        // 判定只由调用方传入的 `now_ms` 驱动——生产是墙钟，回放是虚拟时钟，
        // 同一函数、同一节奏，无墙钟窗口（memory: wallclock-test-flake 处方）。
        // 只观测：warn + 计数，不拦截任何东西。
        if self.probe_summary_due(now_ms) {
            self.probe_run_summary(now_ms);
        } else {
            // 非汇总 tick：汇总读过的窗口自此重新计数。
            self.probe_ticks.clear_window_if_consumed();
        }

        // Persist finalized near-miss records (blocked signals + their paths).
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.take_near_misses(now_ms))
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss persist failed");
            }
        }

        // #173: roll the daily-loss budget at the UTC day boundary BEFORE the
        // exit checks, so a close that lands on the new day is counted in it
        // — and report the state that gates entries: the roll itself (audit
        // trail), a restored/just-tripped breaker, and #177's withheld stops.
        self.roll_daily_budget(now_ms);
        self.emit_daily_trip();

        // Evaluate exits for every open position and place a closing SELL.
        self.run_exit_checks(now_ms)?;
        // Replay fast path: the dirty-token set is consumed by this cycle's
        // valuation — clear it here (not inside run_exit_checks, whose early
        // returns would skip the clear) so it never becomes a permanent
        // over-approximation. Inserts landing between here and the next
        // tick's exit pass are events that arrive after this point by
        // construction, so nothing is lost. #390 (nofast): the same set wakes
        // the deadline-scheduled ladder, so it is cleared here on the nofast
        // path too — after `run_exit_checks` had its look.
        self.fast_dirty_tokens.clear();
        if self.replay_sched() && !self.config.backtest_fast {
            // #390 (merged maintenance): re-arm the single maintenance
            // deadline — the merged pass's own 1 s beat (report stamps are
            // quantised here in the base) plus the earliest position expiry
            // (time-to-expiry rules). Between these, a quiet grid re-observes
            // the previous run's inputs; dirty tokens wake the pass early on
            // their own.
            self.fast_next_exit_deadline_ms = self
                .positions
                .next_maintenance_deadline_ms(now_ms, EXIT_REPORT_BEAT_MS)
                .unwrap_or(i64::MAX);
        }

        // #177: a protective stop the wick guard withheld is an event, not a
        // silent no-op — review must be able to see "should have triggered".
        self.emit_suppressed_stops();

        // #457: an order resting on an EXPIRED round can never fill — close it
        // BEFORE the escalation sweep, so a dead round is never chased.
        // Live-clock only (see the sweep's own replay note).
        self.run_expiry_sweep(now_ms);
        // E-VA companion: a pair leg whose partner is gone (filled one way,
        // cancelled the other) past the landing grace window is a naked leg —
        // alert once, flatten it. Governed atomic scopes only; live path
        // only (the nofast replay loop keeps its own shape).
        self.run_pair_orphan_sweep(now_ms);
        // Escalate due maker_then_taker orders: cancel maker, cross as taker.
        // Replay fast path: the sweep over every live order runs only when
        // the order set changed, or when the earliest armed escalation
        // deadline has arrived — between those, no `escalate_at_ms` can be
        // due (arming never moves a deadline earlier than the sweep that
        // would notice it recomputes the floor).
        if !self.replay_sched() || self.fast_orders_dirty || now_ms >= self.fast_next_escalation_ms
        {
            self.run_escalation_sweep(now_ms)?;
            if self.replay_sched() {
                self.fast_orders_dirty = false;
                self.fast_next_escalation_ms = self.ome.next_escalation_ms();
            }
        }
        Ok(())
    }

    /// #461 Fix 2/4: the COMPLETE risk verdict an escalated taker leg must
    /// pass before the maker is touched. Both escalation paths (the timer
    /// sweep and the post-only WouldCross reaction) share this ONE function —
    /// there is no third vocabulary.
    ///
    /// Layers, in order:
    ///   1. `check_with_equity` with the REAL account balance — the bare
    ///      `risk.check` the paths used before passed equity 0, which
    ///      silently disabled the #202 equity-relative cap at the exact
    ///      moment the order chases the worst price on the book;
    ///   2. `judge_entry` (the nine systemic limits) when any bound is armed
    ///      — the same verdict the arbitration pipeline hands strategy
    ///      entries, so chasing cannot outrun a cap the entry obeyed;
    ///   3. the PAIR-COST hard cap: for an entry whose condition already has
    ///      a counterpart position (a maker leg that filled or is filling),
    ///      `pair_paid + chase_cost <= $1.00` — a binary pair redeems exactly
    ///      $1, so a higher total is a GUARANTEED loss, not a risk. The
    ///      "entry gate forbids" comment in `PositionManager::merge_condition`
    ///      asserted this invariant; this is the code that makes it true.
    ///
    /// Returns Err with the reason; the caller keeps the maker resting (timer
    /// path) or books the rejection (WouldCross path) — the escalation intent
    /// survives either way via the normal re-arm/backoff machinery.
    pub(crate) fn escalation_guard(
        &self,
        req: &OrderRequest,
        chase_price: Decimal,
    ) -> Result<(), String> {
        // 1. The ordinary submission gate, with the account's real equity.
        let equity = self
            .accounts
            .ledger_of(&req.account_id)
            .map(|l| l.balance())
            .unwrap_or(Decimal::ZERO);
        if let Err(e) = self.risk.check_with_equity(req, equity) {
            return Err(e.message);
        }
        // 2. The systemic nine. Armed bounds judge this entry exactly as the
        //    pipeline would judge a fresh strategy entry (the aggregates are
        //    built the same way — positions only, the incoming notional on top).
        let limits = self.risk.systemic_snapshot();
        if limits.any_armed() {
            let now_ms = blitzkrieg_market_api::net::now_ms();
            let facts = crate::risk::limits::SystemicFacts {
                open_positions: self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.expires_at_ms > now_ms)
                    .count(),
                open_exposure_usd: self
                    .positions
                    .open_positions()
                    .iter()
                    .map(|p| p.cost_usd)
                    .sum(),
                same_asset_exposure_usd: self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.asset == req.asset)
                    .map(|p| p.cost_usd)
                    .sum(),
                day_realized_usd: self.positions.daily_pnl_total(),
            };
            // Same doctrine as the pipeline gate: a holder's bound stop is
            // the wide containment width the walker will actually enforce.
            let holder = self
                .engine
                .as_ref()
                .is_some_and(|e| e.strategy_holds_to_settlement(&req.strategy));
            let bound_stop_pct = if holder {
                crate::exit_policy::holder_effective_stop_pct(0, &self.config.positions.exit)
            } else {
                crate::exit_policy::effective_stop_pct(
                    self.config.positions.exit.stop_loss_pct,
                    // Time left is unknown on this path; 0 = the bound stop at
                    // its tightest (no time-adjustment relief), the
                    // conservative read.
                    0,
                    &self.config.positions.exit,
                )
            };
            if let crate::risk::limits::EntryJudgment::Reject { detail, .. } =
                crate::risk::limits::judge_entry(
                    limits,
                    &facts,
                    chase_price,
                    req.size,
                    bound_stop_pct,
                )
            {
                return Err(detail);
            }
        }
        // 3. The pair-cost cap. Only entries (BUY) chase; a condition with no
        //    counterpart position has nothing to sum against (a fresh pair is
        //    the strategy's own <1.00 decision). Per-share: one UP share plus
        //    one DOWN share redeems exactly $1.00. 跨账户双腿：分母只看本
        //    账户的对手腿（同一账本内赎回 $1）。
        if req.side == Side::Buy
            && let Some(paid) = self.counterpart_pair_basis(
                &req.condition_id,
                &req.asset,
                &parse_direction(&req.direction),
                &req.account_id,
            )
        {
            let total = paid + chase_price;
            if total > Decimal::ONE {
                return Err(format!(
                    "pair-cost cap: counterpart leg basis {paid}/share + chasing at \
                     {chase_price} = {total} > $1.00 a pair redeems — the merge would \
                     lock in a loss"
                ));
            }
        }
        Ok(())
    }

    /// Escalate due maker_then_taker orders: cancel the resting maker, cross
    /// the remainder as a taker. Extracted from `tick` so the replay fast
    /// path can gate the sweep (order-set dirty OR earliest deadline due)
    /// without duplicating the submission rules.
    pub(crate) fn run_escalation_sweep(&mut self, now_ms: i64) -> CoreResult<()> {
        let due: Vec<EscalationTarget> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.escalate_at_ms.map(|t| t <= now_ms).unwrap_or(false))
            .map(|o| {
                (
                    o.order_id.clone(),
                    o.side,
                    o.size - o.filled_size,
                    o.token_id.clone(),
                    o.condition_id.clone(),
                    o.strategy.clone(),
                    o.asset.clone(),
                    o.direction.clone(),
                    o.round_slot,
                    // The maker's own timeout, rebuilt from when it was
                    // armed: a re-armed clock waits the same interval again.
                    o.escalate_at_ms.unwrap_or(now_ms) - o.submitted_at_ms,
                    // E28 (§9.2): the taker leg spends from the maker's account.
                    o.account_id.clone(),
                )
            })
            .collect();
        // #774 test seam: a fill that lands exactly between the snapshot just
        // taken and the cancel below. Production has no mutation point there
        // (the sweep runs under the core lock), so the regression test stages
        // the fill and it is applied — through the ordinary fill path — here.
        #[cfg(test)]
        if let Some(fill) = self.test_escalation_snapshot_fill.take() {
            self.ingest_fill(fill, now_ms)?;
        }
        for (
            id,
            side,
            remaining,
            token,
            condition,
            strategy,
            asset,
            direction,
            slot,
            timeout,
            account_id,
        ) in due
        {
            if remaining <= Decimal::ZERO {
                continue;
            }
            // Reprice the taker leg from the CURRENT book before touching the
            // maker: the resting limit is a passive price — a live FOK stated
            // there would be killed, so escalating to it just moves the
            // failure. If the book cannot cover the remaining size even at
            // the extreme of the grid, leave the maker resting and re-arm the
            // clock: no venue traffic, the entry intent stays alive.
            let cap = if side == Side::Buy {
                Decimal::new(99, 2)
            } else {
                Decimal::new(1, 2)
            };
            let Some((_, worst)) = self.marketable_walk(side, &token, remaining, cap, now_ms)
            else {
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            };
            // issue 568 阶段一：定时 escalation 的追单是同一入腿事实链的
            // 续段 —— 成交/死亡事实寻址回声明腿（原腿 cancel 的死亡事实已
            // 落账；续段 key 在追单 place 前登记，其 Taker 成交照旧记账）。
            self.pair_book.bind_continuation(
                &strategy,
                account_id.as_str(),
                &token,
                &format!("{id}:escalated"),
            );
            let mut req = OrderRequest {
                token_id: token,
                condition_id: condition,
                side,
                mode: FillPolicy::Taker,
                price: worst,
                size: remaining,
                internal_key: format!("{id}:escalated"),
                strategy,
                asset,
                direction,
                round_slot: slot,
                // E28 (§9.2): the escalated leg spends from the maker's own
                // account.
                account_id: account_id.clone(),
            };
            // The maker is about to be cancelled to make room for this leg, so
            // the FULL risk verdict gets the first word — the same verdict the
            // submission path will apply a moment later (#461: real equity +
            // systemic limits + the pair-cost cap, one function for both
            // escalation paths). `marketable_walk` only asks whether the
            // book HAS depth at the grid's extreme; it does not ask whether
            // this core may afford it. With a per-order notional cap, the
            // affordable price is `cap / size`, and a book that only offers
            // depth above it (a one-sided 0.99 ask is the everyday case) makes
            // a naive escalation cancel the maker and THEN get refused: the
            // entry is destroyed rather than deferred, and the propagated
            // error aborts the whole tick. Skipping instead keeps the resting
            // maker alive and re-arms the clock — the same semantics as the
            // no-depth branch above — while saying so once per order.
            if let Err(reason) = self.escalation_guard(&req, worst) {
                if self.escalation_skips.insert(id.clone()) {
                    tracing::info!(
                        order = %id,
                        strategy = %req.strategy,
                        price = %worst,
                        size = %remaining,
                        reason = %reason,
                        "escalation skipped: the chase is refused by the full risk guard; maker leg kept resting"
                    );
                }
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            }
            self.escalation_skips.remove(&id);
            self.cancel(&id, now_ms)?;
            // issue #774: the snapshot above predates the cancel, and cancelling
            // is a venue round-trip. A fill the venue booked in that window (the
            // event path has already projected it into the OME) moved
            // `filled_size`, so the true remainder is smaller than the one the
            // request above was built with — placing it would overshoot the
            // intended size (total filled > size). Re-read the order AFTER the
            // retire and chase only what is still unfilled.
            //
            // If the late fill consumed the order there is nothing left to
            // chase: place NO replacement and let the escalation end fully
            // filled.
            //
            // What remains uncertain: a fill still in flight at the venue when
            // this read runs (one the OME has not seen yet) is NOT covered — the
            // replacement can still overshoot by that in-flight size. Closing
            // that window needs the venue's cancel acknowledgement and a final
            // fill read after it; that protocol is out of scope here.
            let fresh = self.ome.remaining(&id);
            if fresh <= Decimal::ZERO {
                tracing::info!(
                    order = %id,
                    strategy = %req.strategy,
                    stale_size = %req.size,
                    "escalation finished as fully filled: the maker filled between the snapshot and the cancel; no replacement placed"
                );
                continue;
            }
            req.size = fresh;
            // Past the gate, a placement can still fail (a reservation, a
            // venue refusal). The maker is already gone, so that entry is
            // lost — but ONE dead escalation must not abort the maintenance
            // pass: the remaining due orders still have their turn.
            if let Err(e) = self.place_escalated(req, now_ms) {
                tracing::warn!(
                    order = %id,
                    error = %e,
                    "the escalated leg was refused after the maker was cancelled"
                );
            }
        }
        // Forget the skip marks of orders that are no longer live: the set is
        // keyed by order id, and a fresh id is minted for every order.
        let ome = &self.ome;
        self.escalation_skips
            .retain(|id| ome.get(id).is_some_and(|o| o.status.is_live()));
        Ok(())
    }

    /// Check exits and submit a closing SELL for each triggered position.
    ///
    /// Two sources converge on ONE submission loop so both are subject to the
    /// same rules (live-sell dedup, `sell_shares`, risk/ledger/sign in `place`):
    ///  - the kernel's automated exit policy (`check_exits`), suppressed by
    ///    `auto_exits_enabled = false`;
    ///  - explicit strategy close intents (`StrategySignal`), which like a
    ///    manual flatten are honoured even when automated exits are disabled,
    ///    but still cannot bypass the kill switch/risk/dedup/position checks.
    pub(crate) fn run_exit_checks(&mut self, now_ms: i64) -> CoreResult<()> {
        // Consume this cycle's intents up front: an intent on a token with no
        // open position is dropped, never left to fire on a later position.
        let intents = std::mem::take(&mut self.strategy_exits);
        if self.positions.open_positions().is_empty() {
            return Ok(());
        }

        // MERGE interception (task 2.2): a strategy close intent tagged "merge"
        // is a pair-COLLECTION request, not a market exit. Complete UP+DOWN
        // pairs of the intent's condition are burned into their on-chain
        // collateral ($1 per pair) immediately — no order is placed, no ladder
        // prices it, the on-chain merge IS the fill, exactly like settlement's
        // redemption. Unpaired remainders stay open for settlement; an intent
        // with no complete pair behind it is dropped like every intent.
        // Runs BEFORE any book borrow: a merge is priced by construction
        // ($0.50 per leg), never off a quote.
        // Read the live books through a shared immutable borrow instead of
        // cloning the whole map per tick: the valuate/check_exits closure only
        // needs the few tokens behind open positions, not every mirrored book.
        // (Scoped before the merge interception: the closure must drop before
        // the merge's `&mut self` calls.)
        // Issue #390 (replay scheduling, marks only): the base's every-tick
        // valuate advances `current_price`/`prev_price`/`last_book_ts` before
        // its ladder runs, and consumers priced off the mark between the two
        // passes (strategy-signal exits below) see the fresh quote. The merged
        // replay pass reproduces that ordering with a mark-only refresh —
        // dirty tokens on a skip tick (only their book changed), every
        // position on a run tick — and leaves the counter ratchet to the
        // interval backfill inside the ladder.
        if !self.config.backtest_fast {
            let marks_all = !self.config.replay_scheduling
                || !self.fast_dirty_tokens.is_empty()
                || now_ms >= self.fast_next_exit_deadline_ms;
            // Issue #390: the scalar projection (BookView) instead of the
            // full snapshot — the marks refresh reads best_bid/mid_price/
            // timestamp only, so the two owned level-vector clones and the
            // token String per position per tick are gone. `from_book` does
            // the five Decimal reductions ONCE per mirrored book (the
            // closure runs once per position; identical inputs, Copy output).
            // #462: the timestamp passed is the book's own RECEIVED time —
            // stamping `now_ms` here would make every book age zero and
            // blind the freshness gate to feed outages.
            let book_fn = |token: &str| {
                self.books
                    .get(token)
                    .map(|b| crate::exit_policy::BookView::from_book(b, b.received_ms))
            };
            self.positions
                .refresh_marks(&book_fn, now_ms, marks_all, &self.fast_dirty_tokens);
        }
        let (merge_intents, sell_intents): (Vec<_>, Vec<_>) =
            intents.into_iter().partition(|i| i.reason == "merge");
        for intent in merge_intents {
            // (account, token)-keyed: two scopes can hold the same token and a
            // merge intent must burn the pair the intending scope's account owns.
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(condition_id) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .map(|p| p.condition_id.clone())
            else {
                continue;
            };
            let Some(outcome) = self.positions.merge_condition(&condition_id, now_ms) else {
                tracing::info!(
                    condition = %condition_id,
                    "merge intent: no complete pair to burn (one-sided or cross-account) — legs stay open"
                );
                continue;
            };
            if outcome.pairs_merged > Decimal::ZERO {
                // #461: the burned pairs are a RECEIVABLE, not cash. On-chain
                // the CTF merge mints USDC only when the transaction mines;
                // crediting `balance` at intent time said the wallet held
                // money it did not hold — the audit's venue leg (live) or the
                // local cash identity (dry) goes red, correctly. The claim
                // rides the settlement book's journal, so a restart between
                // the burn and the receipt keeps the $1/pair.
                let neg_risk = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.neg_risk_of(&intent.token_id))
                    .unwrap_or(false);
                let claim_id = self.settlement.note_merge_burned(
                    &outcome.condition_id,
                    outcome.account_id.as_str(),
                    // #647: the venue the burned pairs sit on — the legs' own
                    // execution-venue stamp, so the merge request drains to
                    // the executor that can run this CTF merge (and to no
                    // sibling's bridge).
                    &outcome.venue,
                    neg_risk,
                    outcome.pairs_merged,
                    now_ms,
                );
                tracing::info!(
                    condition = %outcome.condition_id,
                    pairs = %outcome.pairs_merged,
                    claim = %claim_id,
                    "merge burned complete pairs — collateral pending on-chain confirmation"
                );
            }
            self.persist_positions();
            for closed in &outcome.closed {
                self.on_position_closed(closed, now_ms);
            }
            tracing::info!(
                condition = %outcome.condition_id,
                pairs = %outcome.pairs_merged,
                cash_usd = %outcome.cash_credited_usd,
                unpaired = %outcome.unpaired_shares,
                "merged complete pairs into collateral"
            );
        }

        // Always re-value open positions from the latest books so the dashboard's
        // unrealized PnL / HWM move even when automated exits are disabled.
        // Replay fast path (--backtest-fast): when EVERY registered strategy
        // holds to settlement and the kernel settles locally, every ladder
        // verdict against an open position is discarded by the settlement-
        // holders filter below — the ladder is pure per-tick waste. The fast
        // path re-values only dirty tokens and skips the ladder + the E31-b
        // scan entirely (a holder never places a working sell, so the
        // residual backstop has nothing to answer between events). A mixed
        // book (any non-holder registered) keeps the full historical pass.
        let all_hold = self.config.backtest_fast
            && self.config.mode.settles_locally()
            && self
                .engine
                .as_ref()
                .is_some_and(|e| e.all_strategies_hold_to_settlement());
        // Issue #390: the scalar projection (BookView) instead of the full
        // snapshot — the ladder reads best_bid/best_ask/mid_price/depths/
        // timestamp and never the level vectors, so the two owned clones and
        // the token String per position per tick disappear; the reductions
        // run once per mirrored book and the Copy result is passed by value.
        // #462: the timestamp is the book's own RECEIVED time (see the marks
        // closure above) — the ladder must not re-arm a stale quote.
        let book_fn = |token: &str| {
            self.books
                .get(token)
                .map(|b| crate::exit_policy::BookView::from_book(b, b.received_ms))
        };
        if all_hold {
            self.positions
                .valuate_dirty_tokens(&book_fn, now_ms, &self.fast_dirty_tokens);
        } else if !self.config.replay_scheduling || self.config.backtest_fast {
            // Legacy spelling (live kernels and plain --backtest-fast without
            // all-hold): the base's every-tick valuate, unchanged.
            //
            // Issue #390 (replay scheduling): this pass is MERGED into the
            // gated ladder below — its state update there runs at the 2×
            // backfill rate (the base advances the counter ratchet twice per
            // tick: once here, once in the ladder), and the mark-only refresh
            // above carries `current_price`/`prev_price`/`last_book_ts`.
            // Running the full valuate too would double the ratchet again on
            // run ticks and is pure idempotent re-observation on skip ticks.
            self.positions.valuate(&book_fn, now_ms);
        }
        let intents = sell_intents;

        // A unit of closing work resolved against a concrete open position.
        #[derive(Clone)]
        struct ExitJob {
            position_id: String,
            token: String,
            condition_id: String,
            price: Decimal,
            reason: ExitReason,
            use_maker: bool,
            internal_key: String,
            strategy: String,
            asset: String,
            direction: String,
            /// E28 (§9.2): the account the closing SELL spends from — the
            /// position's own book, so an exit can never debit another account.
            account_id: AccountId,
            /// Issue #464: shares to sell (`None` = the whole sellable
            /// position, every path's historical behavior). A ladder step's
            /// tranche rides here from the `ExitRequest`.
            close_shares: Option<Decimal>,
        }

        let mut jobs: Vec<ExitJob> = Vec::new();
        let mut has_job: HashSet<String> = HashSet::new();

        // Hold-to-settlement strategies keep their positions to expiry: selling
        // a complete-set leg before redemption destroys the pair's riskless
        // payoff ($1 per share-pair regardless of which side wins). The policy
        // ladder therefore skips them — but only where the kernel settles
        // locally (dry/read-only), where `drive_settlement` does the redemption.
        // On a LIVE kernel the ladder keeps running until venue resolutions are
        // a proven path: a position sitting unmanaged past expiry would be
        // strictly worse than one sold early. Strategy-signal intents still
        // close such positions everywhere — an explicit "get out" is honoured.
        // Fast path: with every strategy a holder, the set is every strategy —
        // and its only consumers (the ladder loop, skipped above) are gated
        // off, so the per-tick rebuild is skipped too.
        let settlement_holders: HashSet<String> = if !all_hold && self.config.mode.settles_locally()
        {
            self.positions
                .open_positions()
                .iter()
                .map(|p| p.strategy.clone())
                .filter(|s| {
                    self.engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(s))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let holds_settlement =
            |pos: &crate::position::OpenPosition| settlement_holders.contains(&pos.strategy);

        if self.config.auto_exits_enabled && !all_hold {
            // Issue #390 (replay nofast): the ladder runs on its deadline
            // schedule instead of every tick — dirty tokens (a book arrived
            // for a held position), the exit-deadline clock (expiry/
            // force-exit/throttle beats), or the 1 s report beat that keeps
            // every time-quantised alert stamp the every-tick pass produced.
            // The nofast replay observes every verdict, so the schedule below
            // is output-preserving by construction: a skipped tick's only
            // durable effect on the ladder is the counter ratchet, which the
            // interval-compensated state update backfills. Every timestamp
            // still stamps the tick that actually fires.
            // `interval_mode` is the #390 merged-maintenance spelling —
            // nofast replay ONLY. The fast path stays on the legacy
            // per-tick `valuate_one`: its all-holder filter discards every
            // ladder verdict anyway, so the merged pass buys it nothing,
            // while the per-position run ledger the merged pass maintains
            // showed up as a measurable regression (68s → 90s full corpus).
            let (ladder_run, interval_mode) = if self.config.backtest_fast {
                (true, false)
            } else if self.config.replay_scheduling {
                (
                    !self.fast_dirty_tokens.is_empty() || now_ms >= self.fast_next_exit_deadline_ms,
                    true,
                )
            } else {
                (true, false)
            };
            for req in self.positions.check_exits_interval(
                &book_fn,
                now_ms,
                ladder_run,
                interval_mode,
                self.config.tick_schedule_ms,
                &self.fast_dirty_tokens,
            ) {
                let Some(pos) = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| p.id == req.position_id)
                    .cloned()
                else {
                    continue;
                };
                if holds_settlement(&pos) {
                    continue;
                }
                has_job.insert(pos.id.clone());
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price: req.exit_price,
                    reason: req.reason,
                    use_maker: req.use_maker,
                    internal_key: exit_intent_key("exit", &pos, &format!("{:?}", req.reason)),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                    // E28 (§9.2): the automated exit spends from the position's
                    // own account.
                    account_id: pos.account_id.clone(),
                    // #464: a ladder step's tranche (None = the whole
                    // position, every no-ladder path unchanged).
                    close_shares: req.close_shares,
                });
            }
        }

        // Strategy-signal exits: resolve the token to a live position and price
        // off its latest valuation, exactly like a manual flatten. The intent's
        // OWN account is the first key (账户↔venue 解耦: two scopes can hold
        // the same token; the close must land on the intending scope's row),
        // and the job's internal key carries the account segment so two
        // accounts' exit ladders never collide in the place-cooldown/dedup
        // map. A legacy intent (no account — the pre-fanout shape) resolves
        // to `default`, the same row it always hit.
        for intent in intents {
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(pos) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .cloned()
            else {
                continue;
            };
            if !has_job.insert(pos.id.clone()) {
                continue; // an automated/policy exit already closes it this cycle
            }
            let price = if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                Decimal::new(1, 2)
            };
            let tag: String = intent
                .reason
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(32)
                .collect();
            jobs.push(ExitJob {
                position_id: pos.id.clone(),
                token: pos.token_id.clone(),
                condition_id: pos.condition_id.clone(),
                price,
                reason: ExitReason::StrategySignal,
                use_maker: false,
                internal_key: exit_intent_key("exit-strategy", &pos, &tag),
                strategy: pos.strategy.clone(),
                asset: pos.asset.clone(),
                direction: pos.direction.as_str().to_string(),
                // E28 (§9.2): the strategy-signal exit spends from the
                // position's own account.
                account_id: pos.account_id.clone(),
                close_shares: None,
            });
        }

        // E31-b residual backstop: a working exit can be outgrown by its
        // position (a late entry fill lands while the sell is resting), and
        // once the sell completes the residual has NO working sell and the
        // exit policy may stay silent (e.g. the profit target already passed).
        // Any position with a sell that filled after the position opened and
        // no live sell now gets an explicit re-close of the remainder.
        // Fast path: a holder never places a working sell on these tokens,
        // so between dirty cycles there is no residual to re-close — the
        // scan is skipped with the ladder (still runs on the non-fast path
        // and whenever any non-holder strategy is registered).
        // Issue #390 (replay scheduling): the scan's inputs move only when an
        // ORDER event lands (a sell fills, a late entry fills) or when the
        // ladder actually ran (its verdicts can place/complete sells) —
        // gating it to those ticks reproduces the every-tick scan: a quiet
        // tick re-reads the same (live sells, filled-since, open positions)
        // and would emit nothing. `fast_orders_dirty` is the #388 order-set
        // wake, maintained on both replay paths.
        let residual_scan = if self.config.backtest_fast {
            true
        } else if self.config.replay_scheduling {
            self.fast_orders_dirty || now_ms >= self.fast_next_exit_deadline_ms
        } else {
            true
        };
        if self.config.auto_exits_enabled && !all_hold && residual_scan {
            // Issue #464: a ladder tranche's SELL is a PARTIAL exit — while it
            // works, the position has shares left on purpose. The residual
            // backstop below re-closes exactly such remainders, so a ladder
            // book would be re-flattened wholesale one tick after every
            // step. A position whose token has a working or recently filled
            // ladder SELL stays out of this scan until the ladder's own next
            // step (or stop) takes it the rest of the way. No-ladder runs
            // never carry tranches, so their scan is untouched.
            let ladder_tranche_tokens: HashSet<String> = jobs
                .iter()
                .filter(|j| j.close_shares.is_some())
                .map(|j| j.token.clone())
                .collect();
            for pos in self.positions.open_positions().to_vec() {
                if !has_job.insert(pos.id.clone()) {
                    continue;
                }
                if ladder_tranche_tokens.contains(&pos.token_id) {
                    continue;
                }
                let has_live_sell = self
                    .ome
                    .live_for(&pos.token_id, Side::Sell)
                    .into_iter()
                    .any(|o| o.status.is_live());
                if has_live_sell {
                    continue;
                }
                let closed_here = self
                    .ome
                    .has_filled_sell_since(&pos.token_id, pos.entered_at_ms);
                if !closed_here {
                    continue;
                }
                let price = if pos.current_price > Decimal::ZERO {
                    pos.current_price
                } else {
                    Decimal::new(1, 2)
                };
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price,
                    reason: ExitReason::ForceExit,
                    use_maker: false,
                    internal_key: exit_intent_key("exit-residual", &pos, ""),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                    // E28 (§9.2): the residual re-close spends from the
                    // position's own account.
                    account_id: pos.account_id.clone(),
                    close_shares: None,
                });
            }
        }

        for job in jobs {
            let already_live = self
                .ome
                .live_for(&job.token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live());
            if already_live {
                continue;
            }
            // The venue already refused this exit a moment ago: let the
            // backoff run instead of re-POSTing every tick (the 380+-rejection
            // storm this guards against). Silent skip — the rejection was
            // already accounted for when it happened.
            if self.placement_blocked(&job.internal_key, now_ms) {
                continue;
            }
            // #464: a ladder tranche sells exactly its floor-to-grid size
            // (never more than the position still holds); `None` sells the
            // whole sellable position — every no-ladder path, unchanged.
            let size = if let Some(tranche) = job.close_shares {
                let Some(whole) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                tranche.min(whole)
            } else {
                let Some(size) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                size
            };
            let mode = if job.use_maker {
                FillPolicy::Maker
            } else {
                FillPolicy::Taker
            };
            // A FOK sell must fill in FULL at or above its limit; priced at
            // the touch it dies on the first thin tick (thin books race the
            // quote). Selling LOWER only widens the marketable range — the
            // venue still fills at the bid or better, so the slip is a floor
            // for where we agree to sell, not a price we pay.
            //
            // #496: a resting (maker) sell priced AT the bid is a crossing
            // post-only order — the venue refuses it every time, so a live TP
            // exit could never rest (the rejection storm #163 only backs off;
            // dry's close-intent exemption masked it). The resting sell goes
            // one tick ABOVE the bid, clamped to stay strictly inside the
            // ask (a price at/above the ask would cross the other side and
            // be refused too). No bid or no book: there is nothing to rest
            // against, so the job's judged price stands and the venue's own
            // refusal (or the dry book aging) reports it.
            let price = if mode == FillPolicy::Taker {
                marketable_exit_price(job.price, self.config.exit_taker_slip_ticks)
            } else {
                self.maker_exit_price(&job.token, job.price)
            };
            let order = OrderRequest {
                token_id: job.token.clone(),
                condition_id: job.condition_id,
                side: Side::Sell,
                mode,
                price,
                size,
                internal_key: job.internal_key,
                strategy: job.strategy,
                asset: job.asset,
                direction: job.direction,
                round_slot: 0,
                // E28 (§9.2): the closing SELL spends from the position's own
                // account (carried on the job from the position).
                account_id: job.account_id,
            };
            // Record the intended exit reason so a full sell fill closes with it
            // (bounded and cleaned up by the close: issue #190).
            self.note_exit_reason(&job.token, job.reason);
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            }
        }
        Ok(())
    }
}
