use super::*;

impl Core {
    pub fn list_orders(&self) -> Vec<TrackedOrder> {
        self.ome
            .all()
            .into_iter()
            .map(|o| {
                let mut o = o.clone();
                // The order's venue tag is the venue binding of the account it
                // was placed from — the same fact `account.list` reports (the
                // execution routes by it too). Unknown account / unbound → empty
                // (the field is skipped on the wire, the panel keeps the row).
                o.venue = self
                    .accounts
                    .account(&o.account_id)
                    .map(|a| a.venue.clone())
                    .unwrap_or_default();
                o
            })
            .collect()
    }

    /// Reserve/risk-gate/register an order as Pending without simulating a fill.
    /// Used by the live adapter before the async CLOB POST resolves.
    pub fn place_pending(
        &mut self,
        req: OrderRequest,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        if req.side == Side::Buy {
            // Same entry gate as `place`: a pending opening order is an opening
            // order, however it is submitted.
            self.audit_entry_gate()?;
        }
        // The #202 equity-relative cap is a percentage of the account as it is
        // AT SUBMISSION, so the gate is handed the live balance here — the same
        // number the daily-loss breaker opens its day with. E28: the account
        // is the one the request names, and its posture gates the place too.
        self.accounts
            .permits_order(&req.account_id, is_close_intent(&req.internal_key))?;
        // Account-scoped kill switch, same contract as in `place_gated`:
        // frozen account → new orders refused, closes pass.
        self.account_freeze_permits(&req)?;
        // G5: a close settles into the book that PAID for the shares — a
        // close order naming another account is refused, never re-routed.
        self.refuse_cross_account_close(&req)?;
        // Account↔venue binding (first batch), same rule as `place`: an
        // account pinned to venue V may not trade another venue's tokens.
        self.refuse_cross_venue_order(&req)?;
        // Duration-family admission (B1c), same rule as `place_gated`: the
        // account's (venue, duration) subscription set gates the place; an
        // empty set (today's always-shape) admits everything, closes exempt.
        self.refuse_unsubscribed_duration(&req)?;
        self.risk
            .check_with_equity(&req, self.accounts.ledger_of(&req.account_id)?.balance())?;
        let id = self.new_order_id();
        let buy = req.side == Side::Buy;
        let req_account = req.account_id.clone();
        let req_token = req.token_id.clone();
        if buy {
            self.accounts
                .get_mut(&req.account_id)?
                .reserve(&id, req.price * req.size)?;
        }
        if let Err(e) = self.ome.submit(SubmitParams {
            // #351: the pre-submit path settles locally with zero latency
            // dials — decision, execution and report all land here.
            decision_at_ms: now_ms,
            execute_at_ms: now_ms,
            report_at_ms: now_ms,
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
            // The pre-submit path cannot know the entry's escalation window;
            // `confirm_live` falls back to the default clock when it arms.
            maker_timeout_ms: 0,
            // B1b: the order's round-length family, resolved three-state off
            // the token's market and the global grid.
            round_duration_sec: self.round_family_of_token(&req_token),
        }) {
            // #537: a submit refusal happens AFTER the notional reservation —
            // leaving it on the books strands cash on an order that does not
            // exist, and the accounting audit correctly halts entries on the
            // phantom holds. Roll the reservation back with the order.
            if buy && let Some(l) = self.accounts.get_mut(&req_account).ok() {
                l.release(&id);
            }
            return Err(e);
        }
        self.emit_order(&id);
        Ok((id, OrderStatus::Pending))
    }

    /// Venue acknowledged the order (resting live). Optionally map to its venue id.
    pub fn confirm_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            self.ome.mark_live(id, now_ms)?;
            // Release reservation only on fill/cancel; keep it while resting.
            self.emit_order(id);
            // The venue just accepted an order from us: capability proven for
            // this instant, so the failure streak and this intent's cooldown
            // both end here — the global readout AND the account's own
            // streak (its venue proved itself; a sibling venue's streak is
            // none of its business).
            self.consecutive_venue_rejects = 0;
            self.consecutive_venue_rejects_by_account
                .remove(o.account_id.as_str());
            // 冻结自愈：the venue just accepted an order from this account —
            // the strongest proof of life there is. An AUTO freeze rides
            // circumstantial evidence (a rejection streak, a failed probe);
            // positive evidence outranks it. Manual freezes never lift here.
            self.auto_lift_if_venue_healthy(o.account_id.as_str(), "venue accepted order", now_ms);
            self.place_cooldowns.remove(&o.internal_key);
            // A `maker_then_taker` order rests as GTC; the escalation clock
            // starts on the venue's acceptance (the submit path cannot arm it
            // in LIVE — the ack arrives asynchronously). Without this the
            // maker leg rests forever and the taker fallback never fires.
            // A negative entry timeout is the operator's "never escalate"
            // switch: no clock is armed, so the leg stays a pure maker.
            if o.mode == FillPolicy::MakerThenTaker
                && o.escalate_at_ms.is_none()
                && let Some(delay) =
                    escalation_delay_ms(o.maker_timeout_ms, self.config.default_maker_timeout_ms)
            {
                self.ome.set_escalation(id, now_ms + delay)?;
            }
        }
        Ok(())
    }

    /// The venue POST timed out — the venue MAY have accepted the order
    /// (#462). The order stays Pending with its reservation held, and the
    /// periodic reconciliation sweep decides: resting at the venue → adopted
    /// as ours (ghost repair / `known_venue_order_ids`), filled → gap-fill
    /// into the OME, absent from the venue → MarkedCancelled. Rejecting it
    /// here (the old behaviour) would release cash the venue may already have
    /// spent and strand a real BUY as an unmanaged position.
    pub fn note_place_timeout(&mut self, id: &str) {
        let Some((side, submitted_at)) = self
            .ome
            .get(id)
            .filter(|o| o.status.is_live())
            .map(|o| (o.side, o.submitted_at_ms))
        else {
            return;
        };
        tracing::warn!(
            order = %id,
            side = ?side,
            "venue place timed out — order stays pending; reconciliation will decide"
        );
        // Stop the re-submit: the drain is a live VIEW, so an unflagged order
        // is handed to the executor again on the next tick (500 ms) and
        // re-signed as a brand-new order at venues without a client-id dedupe.
        if let Err(e) = self.ome.mark_place_uncertain(id, submitted_at) {
            tracing::warn!(order = %id, error = %e, "could not stamp place-timeout flag");
        }
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Timeout,
            message: format!(
                "place timed out for {id}: kept pending, reconciliation decides its fate"
            ),
        });
    }

    /// Venue rejected/killed an order before acceptance: release any reservation.
    pub fn reject_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        self.reject_live_result(id, None, now_ms)
    }

    /// Same as [`Core::reject_live`], carrying the venue's own failure text so
    /// cooldowns, per-strategy accounting and the freeze decision all classify
    /// against the REAL reason instead of a bare "rejected".
    pub fn reject_live_result(
        &mut self,
        id: &str,
        err: Option<crate::model::CoreError>,
        now_ms: i64,
    ) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            if o.side == Side::Buy {
                // E28: the reservation is released in the ORDER'S OWN book —
                // a venue reject must never refund another account's cash.
                if let Ok(l) = self.accounts.get_mut(&o.account_id) {
                    l.release(id);
                }
                // E-VA: the virtual hold leaves the pot with the real one.
                self.virtual_release_order(&o);
            }
            self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
            // RFC 568 阶段一：死亡事实（venue 拒单）喂配对簿。
            self.pair_on_order_event(&o.internal_key, Decimal::ZERO, true);
            // The rejected entry never rested: its token may be re-attempted.
            if let Some(engine) = self.engine.as_mut() {
                engine.release_pending_token_for_strategy(
                    o.account_id.as_str(),
                    &o.strategy,
                    &o.token_id,
                );
            }
            self.emit_order(id);
            let e = err.unwrap_or_else(|| {
                crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    "venue rejected order",
                )
            });
            // A `maker_then_taker` entry whose maker leg cannot even REST
            // (post-only would cross the book) never gets a turn to escalate
            // by timer — the venue refused the resting order itself. The
            // mode's contract is to cross instead: place the taker leg now.
            // Other failure classes (balance, auth, venue down) would fail as
            // takers too, so they keep the normal backoff/retry path.
            if o.mode == FillPolicy::MakerThenTaker
                && e.code == blitzkrieg_market_api::CoreErrorCode::WouldCross
            {
                // Same contract as the timer escalation: cross at the price
                // the CURRENT book actually offers, not the rejected passive
                // limit. If no crossing depth exists even at the grid's
                // extreme, there is nothing to escalate to — the intent goes
                // to the normal rejection accounting (backoff/freeze) and a
                // later tick re-enters if the signal persists.
                // issue #774: same correction as the timer sweep. `o` is the
                // snapshot cloned BEFORE the order was retired; a fill that
                // reached the OME in the meantime has moved `filled_size`, so
                // re-read the order now and chase only the remainder. A maker
                // the late fill consumed gets NO replacement.
                //
                // What remains uncertain: a fill still in flight at the venue
                // when this read runs (one the OME has not seen yet) is not
                // covered; closing that needs the venue's acknowledgement,
                // out of scope here.
                let remaining = self.ome.remaining(id);
                if remaining <= Decimal::ZERO {
                    tracing::info!(
                        order = %id,
                        strategy = %o.strategy,
                        size = %o.size,
                        "escalation finished as fully filled after WouldCross: no replacement placed"
                    );
                    return Ok(());
                }
                let cap = if o.side == Side::Buy {
                    Decimal::new(99, 2)
                } else {
                    Decimal::new(1, 2)
                };
                if let Some((_, worst)) =
                    self.marketable_walk(o.side, &o.token_id, remaining, cap, now_ms)
                {
                    // issue 568 阶段一：追单是同一入腿事实链的续段 —— 其
                    // 成交/死亡事实必须寻址回声明腿。原腿的 key 已随死亡
                    // 事实落在簿里；续段 key（{id}:escalated）在此登记。
                    self.pair_book.bind_continuation(
                        &o.strategy,
                        o.account_id.as_str(),
                        &o.token_id,
                        &format!("{id}:escalated"),
                    );
                    let req = OrderRequest {
                        token_id: o.token_id.clone(),
                        condition_id: o.condition_id.clone(),
                        side: o.side,
                        mode: FillPolicy::Taker,
                        price: worst,
                        size: remaining,
                        // Only the engine's ENTRY order is `maker_then_taker`
                        // (exits are Maker or Taker), so this escalated leg is
                        // always new exposure and rightly stays subject to the
                        // kill switch — it is not a close intent (P0 #174).
                        internal_key: format!("{id}:escalated"),
                        strategy: o.strategy.clone(),
                        asset: o.asset.clone(),
                        direction: o.direction.clone(),
                        round_slot: o.round_slot,
                        // E28 (§9.2): the escalated leg spends from the same
                        // account as the maker order it replaces.
                        account_id: o.account_id.clone(),
                    };
                    // The FULL risk verdict gets the first word here too (#461:
                    // real equity + systemic limits + the pair-cost cap, the
                    // same guard the timer escalation runs) — a cross the book
                    // offers but this core may not afford is not an escalation,
                    // it is a second refusal — and taking it would spend the
                    // intent's place in the placement history on a leg that
                    // never had a chance. Falling through books the original
                    // venue rejection instead, which is the same handling as
                    // "no crossing depth".
                    match self.escalation_guard(&req, worst) {
                        Ok(()) => match self.place_escalated(req, now_ms) {
                            Ok(_) => {
                                return Ok(());
                            }
                            Err(e2) => self.emit_error(e2),
                        },
                        Err(reason) => tracing::info!(
                            order = %id,
                            strategy = %req.strategy,
                            price = %worst,
                            size = %req.size,
                            reason = %reason,
                            "escalation skipped after WouldCross: the chase is refused by the full risk guard"
                        ),
                    }
                }
            }
            self.note_venue_rejection(
                &o.internal_key,
                o.strategy.clone(),
                o.account_id.as_str(),
                &e,
                now_ms,
            );
        }
        Ok(())
    }

    /// Book a venue rejection: per-strategy accounting (`ordersRejected` /
    /// `rejectionCauses`), the intent's retry cooldown with exponential
    /// backoff, the panel-visible last error, and — once the consecutive
    /// streak hits [`FREEZE_ON_REJECTS`] — the trading freeze itself.
    pub(crate) fn note_venue_rejection(
        &mut self,
        internal_key: &str,
        strategy: String,
        account_id: &str,
        err: &crate::model::CoreError,
        now_ms: i64,
    ) {
        self.stats.venue_rejected += 1;
        // 冻结自愈（2026-10-28 真线事故）：a close intent refused with
        // "Insufficient position" means the venue says the position is ALREADY
        // GONE — almost always the exit POST that timed out actually FILLED.
        // That is benign bookkeeping drift, not a venue defect: feeding it to
        // the freeze streaks froze accounts for hours (5 honest rejections in
        // a row = kill switch). Close intents keep their per-intent backoff
        // below; only the freeze counters are exempt. Opening intents with
        // the same reason stay counted — for them the refusal is a real gate.
        let benign_close = is_close_intent(internal_key) && is_position_already_gone(&err.message);
        if benign_close {
            if !self.place_cooldowns.contains_key(internal_key) {
                tracing::warn!(
                    key = %internal_key,
                    account = %account_id,
                    reason = %err.message,
                    "benign close-intent rejection (Insufficient position) — backoff retry continues, freeze streak not fed"
                );
            }
        } else {
            self.consecutive_venue_rejects = self.consecutive_venue_rejects.saturating_add(1);
        }
        let account_streak = if benign_close {
            0
        } else {
            let streak = self
                .consecutive_venue_rejects_by_account
                .entry(account_id.to_string())
                .or_default();
            *streak = streak.saturating_add(1);
            *streak
        };
        self.note_error(err, now_ms);
        {
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(&strategy, account_id))
                .or_default();
            acc.rejected += 1;
            *acc.rejection_causes
                .entry(format!("venue.{:?}", err.code))
                .or_default() += 1;
        }
        let (attempts, _next_ok) = {
            let cd = self
                .place_cooldowns
                .entry(internal_key.to_string())
                .or_default();
            cd.attempts = cd.attempts.saturating_add(1);
            cd.last_error = err.message.clone();
            cd.next_ok_ms = now_ms + reject_backoff_ms(cd.attempts);
            (cd.attempts, cd.next_ok_ms)
        };
        if benign_close {
            // 良性拒单的 40 哨兵（≈15–20 分钟连续退避重试）：场所账本和本地
            // 簿已经分歧到退出永远落不下去。平仓意图本身仍继续重试（裸露
            // 敞口不能弃），分歧由 #684 的两层兜底收口：过期且场地永不裁决
            // 的持仓由 `run_expiry_fallback_sweep` 平账入历史；操作员可用
            // `positions.force_close` 点名单清（过期市场限定 + manual-ops
            // 审计）。在此之前的哨兵响声仍然值得 —— 对账 sweep 折回成交前，
            // 操作员先听见一次。
            if attempts == BENIGN_CLOSE_DIVERGENCE_ATTEMPTS {
                let message = format!(
                    "close intent {internal_key} account {account_id} rejected {attempts} times \
                     in a row (Insufficient position) — venue state diverged, reconcile sweep \
                     should fold the fill; manual attention advised"
                );
                tracing::error!("{message}");
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                });
            }
        }
        tracing::info!(
            "venue rejected order: key={internal_key} attempts={} next_retry_in={}ms reason={}",
            attempts,
            reject_backoff_ms(attempts),
            err.message
        );
        // The freeze is per ACCOUNT: the rejections came from one venue, so
        // only the accounts that trade it stop entering. The global counter
        // survives for the legacy readout; the trip decision is the
        // account-scoped one.
        if account_streak >= FREEZE_ON_REJECTS {
            self.freeze_account_auto(
                account_id,
                format!(
                    "{account_streak} consecutive venue rejections on account {account_id}; last: {}",
                    err.message
                ),
                now_ms,
            );
        } else if attempts >= MAX_PLACE_ATTEMPTS {
            // One intent keeps bouncing. Entries stop here; closing intents
            // keep retrying past the cap (unhedged exposure may not be
            // abandoned), but both deserve ONE loud panel alert.
            let (first_time, close) = {
                let cd = self
                    .place_cooldowns
                    .get_mut(internal_key)
                    .expect("cooldown entry just written");
                let first = !cd.alerted;
                cd.alerted = true;
                (first, is_close_intent(internal_key))
            };
            if first_time {
                let status = if close {
                    format!(
                        "still retrying (close intent, backoff only) after {attempts} venue rejections: {}",
                        err.message
                    )
                } else {
                    format!(
                        "abandoned after {attempts} venue rejections: {}",
                        err.message
                    )
                };
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message: format!("placement intent {internal_key} {status}"),
                });
            }
        }
    }

    /// Whether an intent is currently blocked from (re)placement: `Some` while
    /// its cooldown is running or its attempt cap is spent. Callers skip
    /// silently — the rejection was already accounted for when it happened.
    pub(crate) fn placement_blocked(&self, internal_key: &str, now_ms: i64) -> bool {
        self.place_cooldowns.get(internal_key).is_some_and(|c| {
            now_ms < c.next_ok_ms
                || (c.attempts >= MAX_PLACE_ATTEMPTS && !is_close_intent(internal_key))
        })
    }

    /// Apply an authoritative venue/user-WS fill (cumulative size) through the
    /// same idempotent ledger + OME path the dry matcher uses, then project the
    /// resulting delta onto the position book.
    pub fn ingest_fill(&mut self, fill: Fill, now_ms: i64) -> CoreResult<()> {
        let outcome = self.ome.apply_fill_report(fill, now_ms)?;
        self.report_fill_outcome(outcome, now_ms);
        Ok(())
    }

    /// Ledger/position projection plus the late-fill signal for one applied fill
    /// (issue #178). A late fill still moves size and cash; what it must NOT do is
    /// rewrite a terminal order status, and it must reach the operator.
    pub(crate) fn report_fill_outcome(&mut self, outcome: FillOutcome, now_ms: i64) {
        if let Some(d) = outcome.delta {
            self.apply_delta_effects(d, now_ms);
        }
        if let Some(late) = outcome.late_fill {
            self.emit_late_fill(&late, now_ms);
        }
        if let Some(ghost) = outcome.ghost_fill {
            self.emit_ghost_fill(&ghost, now_ms);
        }
    }

    /// Surface a fill that landed on an order which had already ended (or that
    /// reports more size than the order could hold). Returns the alert text.
    ///
    /// `ipc/schema.rs` is outside this change's territory, so this rides the
    /// existing `RiskAlert` event rather than adding a dedicated variant; the
    /// message is a greppable `late_fill …` line so an operator, the audit log
    /// and the panel all show the same fact without a new wire type.
    pub fn emit_late_fill(&mut self, late: &LateFill, now_ms: i64) -> String {
        let status = late
            .absorbed_by
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|| "live".to_string());
        let message = format!(
            "late_fill order={} trade={} status={} size={} cumulative={}/{} reported={} overfill={}",
            late.order_id,
            if late.trade_id.is_empty() {
                "-"
            } else {
                &late.trade_id
            },
            status,
            late.size,
            late.cumulative,
            late.order_size,
            late.reported_cumulative,
            late.overfill,
        );
        tracing::warn!(
            order = %late.order_id,
            trade = %late.trade_id,
            status = %status,
            size = %late.size,
            cumulative = %late.cumulative,
            order_size = %late.order_size,
            reported_cumulative = %late.reported_cumulative,
            overfill = late.overfill,
            "fill arrived for an order that had already ended (status kept, size booked)"
        );
        // Static literal at the stream sink (#693 family); the figures are in
        // the structured line above and the RiskAlert below.
        eprintln!("core: late fill on an ended order — see the warn log line above");
        self.emit(Event::RiskAlert {
            code: if late.overfill {
                // The venue thinks more shares exist than the order asked for:
                // over-delivery, the one case that can exceed our own books.
                CoreErrorCode::RiskRejected
            } else {
                CoreErrorCode::Internal
            },
            message: message.clone(),
        });
        self.persist_audit_value(serde_json::json!({
            "atMs": now_ms,
            "event": "late_fill",
            "orderId": late.order_id,
            "tradeId": late.trade_id,
            "absorbedBy": status,
            "size": late.size.to_string(),
            "cumulative": late.cumulative.to_string(),
            "orderSize": late.order_size.to_string(),
            "reportedCumulative": late.reported_cumulative.to_string(),
            "overfill": late.overfill,
        }));
        message
    }

    /// Surface a fill report that resolved to NO order of ours (issue 691).
    /// Same shape as [`Core::emit_late_fill`]: the existing `RiskAlert` event
    /// (no new wire type), a greppable `ghost_fill …` message, and one audit
    /// row so the shares that moved at the venue stay on the record. The OME
    /// only flags the FIRST sight of a trade key and clears it once the fill
    /// resolves, so re-reports stay quiet and a late-resolving trade stops
    /// alerting on its own.
    pub fn emit_ghost_fill(&mut self, ghost: &crate::ome::GhostFill, now_ms: i64) -> String {
        let message = format!(
            "ghost_fill trade={} token={} side={} size={} price={} — no order to attribute; buffered for retry",
            if ghost.trade_id.is_empty() {
                "-"
            } else {
                &ghost.trade_id
            },
            ghost.token_id,
            ghost.side.as_str(),
            ghost.size,
            ghost.price,
        );
        tracing::warn!(
            trade = %ghost.trade_id,
            token = %ghost.token_id,
            side = ?ghost.side,
            size = %ghost.size,
            price = %ghost.price,
            "fill report resolved to no tracked order — audited and alerted (issue 691)"
        );
        // Static literal at the stream sink (#693 family); the figures are in
        // the structured line above and the RiskAlert below.
        eprintln!("core: ghost fill — see the warn log line above");
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: message.clone(),
        });
        self.persist_audit_value(serde_json::json!({
            "atMs": now_ms,
            "event": "ghost_fill",
            "tradeId": ghost.trade_id,
            "tokenId": ghost.token_id,
            "side": ghost.side.as_str(),
            "size": ghost.size.to_string(),
            "price": ghost.price.to_string(),
        }));
        message
    }

    /// Ledger + position projection for a canonical fill delta. Single choke
    /// point shared by dry fills, live user-WS fills and reconciliation gaps.
    ///
    /// E17: the fee follows the fill's RESOLVED role (`d.role`), and the cash
    /// movement and the position accrual are computed from ONE fee value, so the
    /// ledger and the trade record cannot disagree about what was paid.
    ///
    /// #181: this is also where the BUY reservation is retired. Two numbers move:
    /// the CASH (the execution price that actually left) and the RESERVATION (the
    /// limit-price notional those shares had reserved up front). They differ on a
    /// price-improved fill, and then the reservation is re-synced to the order's
    /// real outstanding commitment — so a full fill, a partial fill and a
    /// terminal transition all leave it exactly right rather than leaving residue
    /// behind (which no cancel path would ever release: a Filled order never gets
    /// cancelled).
    pub(crate) fn apply_delta_effects(&mut self, d: FillDelta, now_ms: i64) {
        let px = d.price;
        // RFC 568 阶段一：配对簿的成交事实喂入（观察者、非判定输入）。
        // FillDelta 不携带 internal_key —— 经 ome.get 取 TrackedOrder 的
        // internal_key；BUY 入腿的正 delta 才入账（SELL 是退出，回滚不
        // 改成交事实的正累计）。未命中声明腿的事件是空操作。
        if d.side == Side::Buy
            && d.delta > Decimal::ZERO
            && let Some(order) = self.ome.get(&d.order_id)
        {
            let key = order.internal_key.clone();
            self.pair_on_order_event(&key, d.delta, false);
        }
        // Replay fast path: a fill advances (or retires) a live order and
        // moves the position book — the escalation floor and the settlement
        // watch may both change on the next tick. It is also an ENGINE INPUT:
        // the strategy's merge-intent emission keys on position completeness,
        // and the equity push feeds sizing — the next evaluation must run so
        // its output lands at the same virtual instant the every-tick pass
        // would have produced it.
        if self.replay_sched() {
            self.fast_path.fast_orders_dirty = true;
            self.fast_path.fast_eval_wake = true;
        }
        // The fee reads only the schedule in force (an &self read), so it is
        // computed BEFORE the account's ledger is taken mutably (E28: the two
        // borrows must not overlap).
        let mut fee_usd = Decimal::ZERO;
        // The modeled maker rebate (almach REPORT §5 失真点 5) reads the same
        // schedule, so it is decided in the same &self window.
        //
        // ponytail: the official rule pays a market's makers 20% of the
        // TAKER-POOL that market accrued, pro-rata by maker share. A replay
        // never sees the pool, so the smallest honest approximation prices
        // THIS fill's own notional at the schedule in force —
        // `0.20 × fee_per_share(px) × shares` — i.e. "the venue's average fee
        // density equals this fill's density". Upgrade path: carry a
        // per-market `taker_fees_accrued / maker_shares_traded` accumulator
        // (pool pro-rata) once a corpus exposes real pool data; the
        // CashFlows.maker_rebate_usd channel below would not change.
        let mut maker_rebate_usd = Decimal::ZERO;
        // #779: a VENUE fill price out of band must not panic the order path.
        // ONE guarded signed notional serves both the fee block and the cash
        // settlement below; on overflow the cash/fee/rebate terms are SKIPPED
        // (logged loudly, never booked at a fabricated value). The reservation
        // resync further down reads the order's own price/remaining and is
        // authoritative, so a skipped term here cannot corrupt the release.
        let notional = match px.checked_mul(d.delta) {
            Some(notional) => notional,
            None => {
                tracing::error!(
                    order = %d.order_id,
                    price = %px,
                    delta = %d.delta,
                    "fill notional overflow — cash/fee/rebate terms skipped (#779)"
                );
                Decimal::ZERO
            }
        };
        // #779: the reserved basis release multiplies the venue-trusted LIMIT
        // price as well. On overflow skip the release (0) — the
        // `sync_buy_reservation` below re-derives the order's true outstanding
        // commitment, so the reservation table still lands exactly right.
        let reserved_release = match d.limit_price.checked_mul(d.delta) {
            Some(release) => release,
            None => {
                tracing::error!(
                    order = %d.order_id,
                    limit_price = %d.limit_price,
                    delta = %d.delta,
                    "fill reservation release overflow — released 0; the resync below \
                     lands the true commitment (#779)"
                );
                Decimal::ZERO
            }
        };
        if d.delta > Decimal::ZERO {
            let fee_pct = if d.role.is_maker() {
                Decimal::ZERO
            } else {
                self.live_taker_fee_pct(px)
            };
            fee_usd = match fee_pct
                .checked_div(Decimal::ONE_HUNDRED)
                .and_then(|rate| rate.checked_mul(notional))
            {
                Some(fee) => fee,
                None => {
                    tracing::error!(
                        order = %d.order_id,
                        fee_pct = %fee_pct,
                        notional = %notional,
                        "fill fee overflow — fee term skipped (#779)"
                    );
                    Decimal::ZERO
                }
            };
            if d.role.is_maker() && self.config.model_maker_rebate {
                let schedule = crate::exit_policy::fee_schedule();
                maker_rebate_usd = match schedule
                    .fee_per_share(px)
                    .checked_mul(dec!(0.20))
                    .and_then(|rebate| rebate.checked_mul(d.delta))
                {
                    Some(rebate) => rebate,
                    None => {
                        tracing::error!(
                            order = %d.order_id,
                            price = %px,
                            delta = %d.delta,
                            "maker rebate overflow — rebate term skipped (#779)"
                        );
                        Decimal::ZERO
                    }
                };
            }
        }
        // E28 (§9.3): every cash movement below lands in the fill's OWN book —
        // the delta carries the account its tracked order was placed with.
        // An unknown account here is a data-integrity fault (a fill arrived
        // for an account this core never configured): it is logged and the
        // fill is dropped rather than silently minting an implicit book
        // (reverse acceptance C) — the position projection and the event
        // emission below are skipped with it.
        let Ok(ledger) = self.accounts.get_mut(&d.account_id) else {
            tracing::error!(
                order = %d.order_id,
                account = %d.account_id.as_str(),
                "fill for an unknown account dropped — the money fact is not booked"
            );
            return;
        };
        // A maker fill pays no fee; a taker fill pays the schedule in force on the
        // fill's own notional. `d.role` is what the fill actually did — the old
        // `match d.mode` read the REQUESTED policy, which charged a
        // MakerThenTaker order the taker fee while its position record said maker.
        // Fee applies to the incremental delta (not the cumulative), so partial
        // fills charge exactly once per share; rollbacks (delta <= 0) charge none.
        if d.delta > Decimal::ZERO {
            match d.side {
                Side::Buy => {
                    // Release the notional reserved at the LIMIT price for these
                    // shares, and pay the execution price in cash.
                    ledger.settle_buy_fill(&d.order_id, notional, reserved_release);
                    ledger.charge_fee(fee_usd);
                }
                Side::Sell => ledger.settle_sell_fill(notional, fee_usd),
            }
            // A maker fill's modeled rebate rides the same journal as #388's
            // stream rows (`Cashflow`, ref `rebate`): it is venue cash that
            // arrived OUTSIDE the fill's own notional/fee movement, and the
            // trade record's `net` claims exactly this amount when the
            // position closes — the two books cannot drift.
            if maker_rebate_usd > Decimal::ZERO {
                ledger.credit_cashflow(maker_rebate_usd, "rebate");
                self.stats.modeled_maker_rebate_usd += maker_rebate_usd;
            }
        } else {
            // Rollback/correction: revert the cash with no fee.
            match d.side {
                Side::Buy => ledger.settle_sell_fill(-notional, Decimal::ZERO),
                // A reversed SELL gives the cash back to the venue; no reservation
                // is involved (SELLs never reserve), so the release is zero.
                Side::Sell => {
                    ledger.settle_buy_fill(&d.order_id, -notional, Decimal::ZERO);
                }
            }
        }
        // Re-sync the reservation to the order's true outstanding commitment: the
        // filled shares are no longer committed (zero when the order ended); a
        // BUY rollback puts them back.
        if d.side == Side::Buy {
            let target = match self.ome.get(&d.order_id) {
                Some(o) if o.status.is_live() => o.price * self.ome.remaining(&d.order_id),
                _ => Decimal::ZERO,
            };
            let before = ledger.sync_buy_reservation(&d.order_id, target);
            if before > Decimal::ZERO && target == Decimal::ZERO {
                tracing::debug!(
                    order = %d.order_id,
                    released = %before,
                    "buy reservation retired (order no longer has an outstanding commitment)"
                );
            }
            // E-VA: the fill converts its own slice of the VIRTUAL hold into
            // position occupancy — released at the LIMIT price (the basis the
            // shares were reserved against, not the execution price), for the
            // signed share delta so a rollback re-arms the hold by the same
            // amount. Positive delta only: a rollback's re-arm happens
            // through the position book (the next cycle's batch check counts
            // the position), not by re-reserving here.
            if d.delta > Decimal::ZERO
                && let Some(order) = self.ome.get(&d.order_id)
            {
                let account = order.account_id.clone();
                let strategy = order.strategy.clone();
                let internal_key = order.internal_key.clone();
                // #779: same guarded LIMIT-price basis as the ledger release.
                self.virtual_accounts.release(
                    account.as_str(),
                    &strategy,
                    &internal_key,
                    reserved_release,
                );
            }
        }
        self.project_fill_delta(&d, fee_usd, maker_rebate_usd, now_ms);
        self.emit_fill(d);
        // The idempotency table changed with this fill; get it on disk before the
        // next event can arrive (issue #178).
        self.persist_applied_journal();
    }

    /// Keep the position book in sync with fills: BUY opens/averages-in, SELL
    /// reduces/closes. One position per (ACCOUNT, token) — 账户↔venue 解耦:
    /// two accounts running the same strategy on the same token are two
    /// independent books, so the token alone no longer identifies the row
    /// (the old single-key merge landed paper's fill onto default's position
    /// and left `count_positions("paper") == 0`). The account rides the
    /// `FillDelta` (E28: copied from the tracked order), so every legacy
    /// caller that never named an account still keys to `default` — the
    /// single-book behaviour is byte-identical.
    ///
    /// `fee_usd` is the cash fee the ledger just charged for this same delta, so
    /// the position's cost basis is the money actually spent (E17-d).
    ///
    /// `maker_rebate_usd` is the modeled rebate the ledger just CREDITED for
    /// this same delta (zero for a taker fill, a rollback, or a replay with
    /// `model_maker_rebate` off). Accrued onto the position's flows so the
    /// close's `net` includes it — the trade record and the cash journal show
    /// one number.
    pub(crate) fn project_fill_delta(
        &mut self,
        d: &FillDelta,
        fee_usd: Decimal,
        maker_rebate_usd: Decimal,
        now_ms: i64,
    ) {
        let px = d.price;
        let token = &d.token_id;
        let account = &d.account_id;
        // Replay fast path: a fill is the one thing that changes the position
        // set (open/close/drop) and the token's mark. #390: the nofast
        // replay's deadline-scheduled ladder reads the same marker — a fill
        // is a book-level input to the ladder's next verdict.
        if self.replay_sched() {
            self.fast_path.fast_positions_dirty = true;
        }
        self.fast_path.fast_dirty_tokens.insert(token.clone());
        match d.side {
            Side::Buy => {
                let existing = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token && &p.account_id == account)
                    .map(|p| p.id.clone());
                let id = match existing {
                    Some(id) => id,
                    None => {
                        let direction = parse_direction(&d.direction);
                        // B1b: the fill's own order carries the family it was
                        // admitted under; the three-state eff (own stamp >
                        // market declaration > global grid) projects the slot
                        // onto the family grid the ORDER actually traded, not
                        // the configured grid. Single-family equivalence: a
                        // deployment on the configured grid alone folds eff ==
                        // cfg, byte-for-byte the historic formula. (Falls back
                        // to the global grid when the order is unknown to the
                        // OME — a synthetic/reconciliation fill.)
                        let eff_sec = self
                            .ome
                            .get(&d.order_id)
                            .map(|o| self.order_eff_round_sec(o))
                            .filter(|eff| *eff > 0)
                            .unwrap_or(self.config.round_duration_sec);
                        // Segment 2 of the wiring template: the lazy stamp —
                        // the order's family is written from the resolution
                        // this projection just made (idempotent; a non-0
                        // stamp is never overwritten).
                        self.ome.backfill_round_duration(&d.order_id, eff_sec);
                        // A round slot N covers [N*dur, (N+1)*dur): the market
                        // expires at the END of the slot. (Using N*dur would place
                        // expiry in the past and force-exit immediately.)
                        let expires_at_ms = if d.round_slot > 0 {
                            (d.round_slot + 1) * eff_sec * 1000
                        } else {
                            now_ms + eff_sec * 1000
                        };
                        let p = OpenParams {
                            strategy: d.strategy.clone(),
                            asset: d.asset.clone(),
                            direction,
                            token_id: d.token_id.clone(),
                            condition_id: d.condition_id.clone(),
                            entry_price: d.price,
                            expires_at_ms,
                            was_maker: d.role.is_maker(),
                            target_exit_price: None,
                            // Slot matching (not a countdown) judges this
                            // position's expiry later: stamp the entry
                            // order's own grid slot.
                            round_slot: d.round_slot,
                            // E28 (§9.2): the position belongs to the account
                            // whose fill opened it.
                            account_id: d.account_id.clone(),
                            // #427: the panels' Venue column, never a decision
                            // input. The stamp is the ORDER'S EXECUTION venue
                            // — under the (venue, token) dual-key table the
                            // stamp equals the fill's own row's venue; for a
                            // shared token the placing account's binding
                            // picks its own row (the same read the executor
                            // drain routes by). Pre-M1 it was a correction
                            // over the token-unique table's first-row venue:
                            // without it the position/trade rows carried a
                            // venue tag the account-bound venue filter hid —
                            // the history badge counted a trade no row ever
                            // showed.
                            venue: self.execution_venue_of(d.account_id.as_str(), &d.token_id),
                            // The condition's other token, stamped from the
                            // SAME market lookup: the dry settlement ladder's
                            // counter-evidence key. Empty when no market is
                            // live for the token — the ladder then has no
                            // partner to consult and withholds as before.
                            partner_token_id: self
                                .engine
                                .as_ref()
                                .and_then(|e| e.partner_of(&d.token_id))
                                .unwrap_or_default(),
                            // B1b: the family the entry was admitted under —
                            // the order's own resolution (its stamp, else the
                            // market declaration, else the grid), so the
                            // position and the order that opened it carry the
                            // SAME family.
                            round_duration_sec: eff_sec,
                        };
                        self.positions.open(p, now_ms).id
                    }
                };
                // Accrue the basis, the fee and the role from the actual fill.
                // `d.delta` is SIGNED: a FAILED rollback reverses the accrual by
                // the same amounts the ledger just refunded, so a rolled-back
                // entry leaves neither cash nor shares behind. The modeled maker
                // rebate (positive deltas only — the rollback's credit_cashflow
                // never fired) accrues onto the same flows.
                self.positions
                    .apply_entry_fill(&id, d.delta, px, fee_usd, d.role);
                if maker_rebate_usd != Decimal::ZERO {
                    self.positions.accrue_maker_rebate(&id, maker_rebate_usd);
                }
                // A fully rolled-back entry has no shares and no basis: drop the
                // shell so it cannot block `can_open` or render as an empty row.
                self.positions.drop_if_empty(&id);
                self.persist_positions();
            }
            Side::Sell => {
                let found = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token && &p.account_id == account)
                    .map(|p| p.id.clone());
                match found {
                    Some(id) => {
                        // F4: the row BEFORE this exit fill accrues is the exact
                        // inventory a later FAILED status for this trade must
                        // restore (after apply_exit_fill the row is a zeroed
                        // shell — useless for a restore).
                        let pre_exit = self
                            .positions
                            .open_positions()
                            .iter()
                            .find(|p| p.id == id)
                            .cloned();
                        // Accrue the exit first, so the close reads final flows.
                        let left = self.positions.apply_exit_fill(
                            &id,
                            d.delta,
                            px,
                            fee_usd,
                            d.role,
                            maker_rebate_usd,
                        );
                        // A reversal that is already accounted for on a CLOSED
                        // trade has nothing open to accrue onto; the cash is
                        // refunded but the realized record stands. Say so — a
                        // silent drop here would be exactly the drift E17 exists
                        // to remove.
                        if d.delta < Decimal::ZERO && left.is_none() {
                            self.emit(Event::Error {
                                error: CoreError::new(
                                    CoreErrorCode::Internal,
                                    format!(
                                        "exit rollback {} for token {token} has no open position to reverse",
                                        d.order_id
                                    ),
                                ),
                            });
                            return;
                        }
                        match left {
                            // Sub-grid remainder: nothing sellable is left, so the
                            // position is done — it must close, or it would sit on
                            // the books forever with an untradeable stub of shares.
                            Some(left)
                                if left <= Decimal::ZERO
                                    || crate::position::floor_to_grid(left) == Decimal::ZERO =>
                            {
                                let reason =
                                    self.take_exit_reason(token).unwrap_or(ExitReason::Manual);
                                let was_maker = d.role.is_maker();
                                let streak_before = self.breaker.consecutive_losses(&d.strategy);
                                // E26 (§4.2): the account streak's count just
                                // before this close, from the SAME map — the
                                // evidence a FAILED rollback needs, captured
                                // whether or not the bound is armed.
                                let account_streak_before = self.breaker.consecutive_losses(
                                    &crate::risk::limits::account_breaker_key(
                                        d.account_id.as_str(),
                                    ),
                                );
                                if let Some(closed) =
                                    self.positions.close(&id, px, reason, was_maker, now_ms)
                                {
                                    if let Some(restored) = pre_exit {
                                        self.push_close_credential(
                                            d.trade_id.clone(),
                                            CloseCredential {
                                                restored,
                                                closed: closed.clone(),
                                                streak_before,
                                                account_streak_before,
                                            },
                                        );
                                    }
                                    self.persist_positions();
                                    self.on_position_closed(&closed, now_ms);
                                }
                            }
                            Some(_) => {
                                self.persist_positions();
                            }
                            None => {}
                        }
                    }
                    None if d.delta < Decimal::ZERO => {
                        // F4: the fill this rollback reverses may have already
                        // fully closed the position and booked the close
                        // (TradeDb / strategy PnL / breaker / daily PnL). Unwind
                        // all of it; only a rollback with NO credential on file
                        // is unexplainable. The credential pool is filtered to
                        // THIS account's positions: two accounts can hold the
                        // same token, and a default leg's rollback must never
                        // unwind paper's close (the (account, token) merge key
                        // above is what makes both rows exist).
                        let consumed = self
                            .close_credentials
                            .iter()
                            .position(|(tid, cred)| {
                                *tid == d.trade_id && cred.restored.account_id == d.account_id
                            })
                            .map(|i| self.close_credentials.remove(i).unwrap().1);
                        // #463: tombstone the consumption so a restart replay
                        // does not resurrect a credential already unwound.
                        if let (true, Some(path)) =
                            (consumed.is_some(), self.close_credential_log.as_ref())
                        {
                            crate::jsonl::append(
                                path,
                                &CloseCredentialLine::Consumed {
                                    trade_id: d.trade_id.clone(),
                                },
                            );
                        }
                        match consumed {
                            Some(cred) => self.unwind_closed_trade(cred, d, now_ms),
                            // No position at all for the token: the reversal cannot
                            // be applied. Never swallow it.
                            None => self.emit(Event::Error {
                                error: CoreError::new(
                                    CoreErrorCode::Internal,
                                    format!(
                                        "exit rollback {} for token {token} has no open position to reverse",
                                        d.order_id
                                    ),
                                ),
                            }),
                        }
                    }
                    None => {}
                }
            }
        }
    }
}
