//! Order lifecycle, fill application and the settlement/redemption family,
//! moved verbatim from `service.rs` (pure code motion, no logic edits):
//! placement bookkeeping (place/confirm/reject), fill projection into the
//! ledger and position book, the pending-close exit-reason table, the
//! close-credential unwind journal, market resolution → redemption/merge
//! plumbing, the expiry sweeps and the dry settlement ladder.

use crate::ipc::schema::Event;
use crate::model::*;
use crate::ome::{FillDelta, FillOutcome, LateFill, SubmitParams};
use crate::position::OpenParams;
use crate::settlement::{
    EXPIRY_FALLBACK_AFTER_MS, SETTLEMENT_EXIT_REASON, UNVERIFIED_EXPIRY_EXIT_REASON, booking_for,
    settlement_key,
};
use crate::sim::{Book, rests_on_book};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;

use super::config::COUNTER_DECISIVE_HIGH;
use super::config::EXIT_REASON_TABLE_MAX;
use super::helpers::{
    BENIGN_CLOSE_DIVERGENCE_ATTEMPTS, FREEZE_ON_REJECTS, MAX_PLACE_ATTEMPTS, dec_json,
    escalation_delay_ms, is_close_intent, is_position_already_gone, reject_backoff_ms,
    strategy_acc_key,
};
use super::{CloseCredential, CloseCredentialLine, Core, parse_direction};

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
        let Some(o) = self.ome.get(id) else {
            return;
        };
        if !o.status.is_live() {
            return;
        }
        tracing::warn!(
            order = %id,
            side = ?o.side,
            "venue place timed out — order stays pending; reconciliation will decide"
        );
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
                let cap = if o.side == Side::Buy {
                    Decimal::new(99, 2)
                } else {
                    Decimal::new(1, 2)
                };
                if let Some((_, worst)) =
                    self.marketable_walk(o.side, &o.token_id, o.size, cap, now_ms)
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
                        size: o.size,
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
    pub(super) fn note_venue_rejection(
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
    pub(super) fn placement_blocked(&self, internal_key: &str, now_ms: i64) -> bool {
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
    pub(super) fn report_fill_outcome(&mut self, outcome: FillOutcome, now_ms: i64) {
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
    pub(super) fn apply_delta_effects(&mut self, d: FillDelta, now_ms: i64) {
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
            self.fast_orders_dirty = true;
            self.fast_eval_wake = true;
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
        if d.delta > Decimal::ZERO {
            let notional = px * d.delta;
            let fee_pct = if d.role.is_maker() {
                Decimal::ZERO
            } else {
                self.live_taker_fee_pct(px)
            };
            fee_usd = (fee_pct / Decimal::ONE_HUNDRED) * notional;
            if d.role.is_maker() && self.config.model_maker_rebate {
                let schedule = crate::exit_policy::fee_schedule();
                maker_rebate_usd = schedule.fee_per_share(px) * dec!(0.20) * d.delta;
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
            let notional = px * d.delta;
            match d.side {
                Side::Buy => {
                    // Release the notional reserved at the LIMIT price for these
                    // shares, and pay the execution price in cash.
                    ledger.settle_buy_fill(&d.order_id, notional, d.limit_price * d.delta);
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
                Side::Buy => ledger.settle_sell_fill(-px * d.delta, Decimal::ZERO),
                // A reversed SELL gives the cash back to the venue; no reservation
                // is involved (SELLs never reserve), so the release is zero.
                Side::Sell => {
                    ledger.settle_buy_fill(&d.order_id, -px * d.delta, Decimal::ZERO);
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
                let limit = d.limit_price;
                let shares = d.delta;
                self.virtual_accounts.release(
                    account.as_str(),
                    &strategy,
                    &internal_key,
                    limit * shares,
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
    pub(super) fn project_fill_delta(
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
            self.fast_positions_dirty = true;
        }
        self.fast_dirty_tokens.insert(token.clone());
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

    /// Breaker + event emission when a position closes.
    pub(super) fn on_position_closed(
        &mut self,
        closed: &crate::position::ClosedPosition,
        now_ms: i64,
    ) {
        // Replay fast path: the position set changed (merge/settlement/exit
        // close all funnel here) — the settlement watch and audit input must
        // resync on the next tick.
        if self.replay_sched() {
            self.fast_positions_dirty = true;
        }
        // The pending-close reason for this token died with the position it
        // belonged to (issue #190). Every close path funnels through here, and
        // an entry left behind would be inherited by the NEXT position on the
        // same token — the ghost reason that mis-attributed exits.
        self.clear_exit_reason(&closed.token_id);
        // Persist first so the panel/analysis have the record even if a later
        // step fails (matches the Node behaviour of saving on close).
        if let Some(db) = self.trade_db.as_mut() {
            let rec = crate::trade_db::TradeRecord::from_closed(closed);
            db.record(&rec, now_ms);
        }
        // Per-strategy accounting (P-1.1): session realized PnL + fee totals,
        // keyed (strategy, account) so the panel's per-account slice sums ONLY
        // the active account's closes (PR H).
        {
            let entry_fee =
                (closed.entry_fee_pct / Decimal::ONE_HUNDRED) * closed.entry_price * closed.shares;
            let exit_fee =
                (closed.exit_fee_pct / Decimal::ONE_HUNDRED) * closed.exit_price * closed.shares;
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(
                    &closed.strategy,
                    closed.account_id.as_str(),
                ))
                .or_default();
            acc.closed_trades += 1;
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins += 1;
            } else {
                acc.losses += 1;
            }
            acc.fees_usd += entry_fee + exit_fee;
            acc.net_pnl_usd += closed.net_pnl_usd;
        }
        // KI-10 / D-18 A: the streak is attributed to the position's own
        // strategy, so only that leg freezes. `PositionClosed` already carries
        // the strategy name, so no new data is needed for attribution.
        let tripped = self
            .breaker
            .record(&closed.strategy, closed.net_pnl_usd, now_ms);
        self.emit(Event::PositionClosed {
            id: closed.id.clone(),
            asset: closed.asset.clone(),
            direction: closed.direction.as_str().to_string(),
            reason: format!("{:?}", closed.exit_reason),
            strategy: closed.strategy.clone(),
            token_id: closed.token_id.clone(),
            condition_id: closed.condition_id.clone(),
            entry_price: closed.entry_price,
            account_id: closed.account_id.clone(),
            net_pnl_usd: closed.net_pnl_usd,
            net_pnl_pct: closed.net_pnl_pct,
            daily_pnl_usd: self.positions.daily_pnl_for(closed.account_id.as_str()),
        });
        if tripped {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "consecutive-loss breaker tripped for strategy {}: {} losses, halting its new entries {}s",
                    closed.strategy,
                    self.breaker.consecutive_losses(&closed.strategy),
                    self.config.breaker_cooldown_sec
                ),
            });
        }
        // E26 (§4.2): the ACCOUNT-level streak lives in the SAME breaker map
        // under the reserved `__account__:` key, with the account matrix's
        // own pair (`max_consecutive_losses`, `cooldown_minutes` → secs; a
        // cooldown the matrix does not name falls back to the shipped
        // `breaker_cooldown_sec`). Off at the factory: nothing is recorded
        // unless the operator armed the bound.
        {
            let account_limits = &self.risk.config().systemic.account;
            let max_losses =
                u32::try_from(account_limits.max_consecutive_losses.value().floor()).unwrap_or(0);
            if max_losses >= 1 {
                let cooldown_sec = if account_limits.cooldown_minutes.is_enabled() {
                    i64::try_from(account_limits.cooldown_minutes.value() * Decimal::from(60))
                        .unwrap_or(self.config.breaker_cooldown_sec)
                } else {
                    self.config.breaker_cooldown_sec
                };
                let account_key =
                    crate::risk::limits::account_breaker_key(closed.account_id.as_str());
                let account_tripped = self.breaker.record_with(
                    &account_key,
                    max_losses,
                    cooldown_sec,
                    closed.net_pnl_usd,
                    now_ms,
                );
                if account_tripped {
                    self.emit(Event::RiskAlert {
                        code: CoreErrorCode::RiskRejected,
                        message: format!(
                            "consecutive-loss breaker tripped for account {}: {} losses, halting its new entries {}s",
                            closed.account_id.as_str(),
                            self.breaker.consecutive_losses(&account_key),
                            cooldown_sec
                        ),
                    });
                }
            }
        }
        // #173: the daily budget freezes entries the moment its cap is reached —
        // reported here (not only on the next tick) so the freeze and the loss
        // land in the same audit window.
        self.emit_daily_trip();
    }

    /// Record the reason an in-flight close of `token` was submitted with.
    pub(super) fn note_exit_reason(&mut self, token: &str, reason: ExitReason) {
        if self
            .exit_reasons
            .insert(token.to_string(), reason)
            .is_none()
        {
            self.exit_reason_order.push_back(token.to_string());
        }
        self.enforce_exit_reason_bound();
    }

    /// Consume the pending reason for `token`: the close that fills uses it, and
    /// it must not outlive that close.
    pub(super) fn take_exit_reason(&mut self, token: &str) -> Option<ExitReason> {
        let reason = self.exit_reasons.remove(token);
        if reason.is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
        reason
    }

    /// Forget the pending reason for `token` without consuming it — the close
    /// that landed carried its own reason (a settlement, a reconciled venue
    /// close), so the in-flight one is stale either way.
    pub(super) fn clear_exit_reason(&mut self, token: &str) {
        if self.exit_reasons.remove(token).is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
    }

    /// Drop every entry whose token no longer has an open position.
    ///
    /// Run when the round rolls over (`engine.markets` replaces the whole round)
    /// and after a close: that is the point at which a leftover entry can no
    /// longer belong to anything live, and an entry whose position is gone would
    /// otherwise be inherited by the next position on that token.
    pub(super) fn sweep_exit_reasons(&mut self) {
        if self.exit_reasons.is_empty() {
            return;
        }
        let open: HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect();
        let stale: Vec<String> = self
            .exit_reasons
            .keys()
            .filter(|t| !open.contains(*t))
            .cloned()
            .collect();
        for token in stale {
            self.exit_reasons.remove(&token);
            self.exit_reasons_swept += 1;
        }
        self.repair_exit_reason_order();
    }

    /// Keep the table within [`EXIT_REASON_TABLE_MAX`], evicting what is least
    /// likely to still be needed: first the entries whose token has no open
    /// position (stale by construction), then the oldest.
    pub(super) fn enforce_exit_reason_bound(&mut self) {
        if self.exit_reasons.len() <= EXIT_REASON_TABLE_MAX {
            return;
        }
        let open: HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect();
        let ghosts: Vec<String> = self
            .exit_reason_order
            .iter()
            .filter(|t| !open.contains(*t))
            .cloned()
            .collect();
        for token in ghosts {
            if self.exit_reasons.len() <= EXIT_REASON_TABLE_MAX {
                break;
            }
            self.exit_reasons.remove(&token);
            self.exit_reasons_evicted += 1;
        }
        while self.exit_reasons.len() > EXIT_REASON_TABLE_MAX {
            let Some(oldest) = self.exit_reason_order.pop_front() else {
                break;
            };
            if self.exit_reasons.remove(&oldest).is_some() {
                self.exit_reasons_evicted += 1;
            }
        }
        self.repair_exit_reason_order();
    }

    /// Drop order entries whose reason is no longer in the map, so the deque
    /// always describes exactly the map's key set (the invariant the eviction
    /// path relies on to name the oldest entry).
    pub(super) fn repair_exit_reason_order(&mut self) {
        self.exit_reason_order
            .retain(|t| self.exit_reasons.contains_key(t));
        debug_assert_eq!(
            self.exit_reason_order.len(),
            self.exit_reasons.len(),
            "exit_reason_order must mirror exit_reasons"
        );
    }

    /// The pending-close table as the panel reads it (issue #190): how many
    /// reasons are held, the bound, and what the two clean-up paths removed.
    /// `orphaned` counts entries whose position is gone — the ghost this table
    /// used to accumulate; it is 0 at rest.
    pub(super) fn exit_reasons_view(&self) -> serde_json::Value {
        let open: HashSet<&str> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.as_str())
            .collect();
        let orphaned = self
            .exit_reasons
            .keys()
            .filter(|t| !open.contains(t.as_str()))
            .count();
        serde_json::json!({
            "tracked": self.exit_reasons.len(),
            "capacity": EXIT_REASON_TABLE_MAX,
            "orphaned": orphaned,
            "evictedTotal": self.exit_reasons_evicted,
            "sweptTotal": self.exit_reasons_swept,
        })
    }

    /// The orderbook freshness budget as the panel reads it (issue #205). The
    /// engine's own value wins when an engine is installed (a backtest or a
    /// replay configures its own); without one the configured value is reported,
    /// which is what the next `install_engine` will apply.
    pub(super) fn orderbook_freshness_view(&self) -> serde_json::Value {
        let max_stale_ms = self
            .engine
            .as_ref()
            .map(|e| e.max_orderbook_stale_ms())
            .unwrap_or(self.config.max_orderbook_stale_ms);
        serde_json::json!({
            "maxStaleMs": max_stale_ms,
            // `maxStaleMs: 0` is not "a zero budget" (which would refuse every
            // book the instant it is stamped): it is the explicit "freshness
            // check OFF". Named here so the panel never renders the two alike.
            "checkEnabled": max_stale_ms > 0,
        })
    }

    /// F4: file a close-accounting credential under a trade key. FIFO-capped —
    /// the FAILED that consumes a credential normally arrives seconds after the
    /// fill that created it, so a small window is all the pairing needs.
    /// #463: the push is journalled so the window survives a restart.
    pub(super) fn push_close_credential(&mut self, trade_id: String, cred: CloseCredential) {
        if trade_id.is_empty() {
            return;
        }
        while self.close_credentials.len() >= 512 {
            let (evicted, _) = self
                .close_credentials
                .pop_front()
                .expect("len checked above");
            // The journal records the eviction as consumed, so a replay after a
            // restart does not resurrect a credential the cap already dropped.
            if let Some(path) = &self.close_credential_log {
                crate::jsonl::append(path, &CloseCredentialLine::Consumed { trade_id: evicted });
            }
        }
        if let Some(path) = &self.close_credential_log {
            crate::jsonl::append(
                path,
                &CloseCredentialLine::Push {
                    trade_id: trade_id.clone(),
                    cred: Box::new(cred.clone()),
                },
            );
        }
        self.close_credentials.push_back((trade_id, cred));
    }

    /// F4: a FAILED status arrived for a trade whose sell fill had already
    /// fully closed a position and booked the close. Reverse every ledger that
    /// close touched, in the order the close applied them:
    ///   1. position inventory — restore the exact open row (shares, basis,
    ///      flows) and hand back the daily PnL the close added;
    ///   2. TradeDb — retract the record (JSONL line dropped, summary rebuilt);
    ///   3. per-strategy accounting — counts, fees and net PnL back out;
    ///   4. consecutive-loss breaker — remove the loss / restore the streak
    ///      the win had reset, lifting a halt this close caused.
    ///
    /// Cash needs nothing here: `apply_delta_effects` already refunded it
    /// before projecting this rollback.
    pub(super) fn unwind_closed_trade(
        &mut self,
        cred: CloseCredential,
        d: &FillDelta,
        now_ms: i64,
    ) {
        let closed = &cred.closed;
        let restored = self.positions.restore_closed(cred.restored, closed);
        if restored {
            self.persist_positions();
        }
        if let Some(db) = self.trade_db.as_mut() {
            let rec = crate::trade_db::TradeRecord::from_closed(closed);
            db.retract(&rec, now_ms);
        }
        {
            let entry_fee =
                (closed.entry_fee_pct / Decimal::ONE_HUNDRED) * closed.entry_price * closed.shares;
            let exit_fee =
                (closed.exit_fee_pct / Decimal::ONE_HUNDRED) * closed.exit_price * closed.shares;
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(
                    &closed.strategy,
                    closed.account_id.as_str(),
                ))
                .or_default();
            acc.closed_trades = acc.closed_trades.saturating_sub(1);
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins = acc.wins.saturating_sub(1);
            } else {
                acc.losses = acc.losses.saturating_sub(1);
            }
            acc.fees_usd -= entry_fee + exit_fee;
            acc.net_pnl_usd -= closed.net_pnl_usd;
        }
        self.breaker
            .retract(&closed.strategy, closed.net_pnl_usd, cred.streak_before);
        // E26 (§4.2): the ACCOUNT-level streak unwinds symmetrically — the
        // same retract, against the reserved `__account__:` key with the
        // count captured before the close. A reversed loss drops the account
        // streak (lifting a halt its own threshold caused); a reversed win
        // restores the streak the win had reset.
        self.breaker.retract(
            &crate::risk::limits::account_breaker_key(closed.account_id.as_str()),
            closed.net_pnl_usd,
            cred.account_streak_before,
        );
        tracing::warn!(
            order_id = %d.order_id,
            token = %d.token_id,
            trade = %d.trade_id,
            position = %closed.id,
            inventory_restored = restored,
            net_pnl_reversed = %closed.net_pnl_usd,
            "FAILED trade reversed a full close: position inventory restored, close ledgers unwound"
        );
    }

    // ── Market data (P0 bridge from Node; P3 ingests inside the core) ───────
    pub fn book_snapshot(
        &mut self,
        token_id: &str,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    ) {
        self.books.insert(
            token_id.to_string(),
            Book {
                bids,
                asks,
                received_ms: now_ms,
            },
        );
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.token_id == token_id && rests_on_book(o.mode))
            .map(|o| o.order_id.clone())
            .collect();
        for id in ids {
            self.try_maker_fill(&id, now_ms);
        }
    }

    /// Markets the core is waiting on a verdict for, as queries for the venue.
    /// The plugin drains this in live mode; dry mode answers itself in
    /// [`Core::tick`], so the same code path is exercised without a chain.
    pub fn take_settlement_queries(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::SettlementQuery> {
        self.sync_settlement_watch(now_ms);
        self.settlement.take_queries(now_ms)
    }

    /// Keep the settlement watch list in step with the open book: a market is
    /// watched once a position of ours is past its expiry, and forgotten as soon
    /// as its last position leaves (settled, or closed by an ordinary exit).
    pub(super) fn sync_settlement_watch(&mut self, now_ms: i64) {
        // Replay fast path: the watch list only changes when the position set
        // changes, or when the earliest open position's expiry passes (that is
        // the instant `track_markets` would first admit the market). Between
        // those, the scan re-derives the same list — skip it.
        if self.replay_sched() && !self.fast_positions_dirty && now_ms < self.fast_next_expiry_ms {
            return;
        }
        // #377 (治 #2): markets the archive promised a verdict for are settled
        // by the stream (`DataEvent::Resolution`), so the dry ladder must not
        // query them — a stamped query would wait for an answer that never
        // comes and trip the blind alert. Filtering here removes them from
        // tracking AND from the forget loop's live set (self-healing: a market
        // that slipped into the watch list is dropped on the next sync).
        let all: Vec<crate::position::OpenPosition> = self.positions.open_positions().to_vec();
        let positions: Vec<crate::position::OpenPosition> = all
            .iter()
            .filter(|p| !self.archive_verdicts.contains(&p.condition_id))
            .cloned()
            .collect();
        self.settlement.track_markets(&positions, now_ms);
        let live: HashSet<&str> = positions.iter().map(|p| p.condition_id.as_str()).collect();
        for condition_id in self.settlement.tracked_condition_ids() {
            if !live.contains(condition_id.as_str()) {
                self.settlement.forget_market(&condition_id);
            }
        }
        if self.replay_sched() {
            self.fast_positions_dirty = false;
            // The next expiry wake is the earliest expiry still IN THE
            // FUTURE. An already-expired position needs no wake: it is
            // either tracked (its re-query clock is `next_query_deadline_ms`,
            // a separate term of the settlement gate) or untrackable (the
            // `EXPIRY_TRUST_MS` refusal — re-deriving that refusal every
            // tick produces nothing). Taking the raw minimum kept this gate
            // permanently open once the earliest expiry had passed: the
            // busy-phase sample put `take_settlement_queries` at 95% of
            // tick, nearly all of it this watch-list re-derivation cloning
            // every open position per tick. Computed over the UNFILTERED
            // set: an archive-verdict position's expiry must still wake the
            // sync (its settlement event arrives around then), and letting
            // the minimum collapse to `i64::MAX` would reopen this gate on
            // every tick — the exact hot path #375 closed.
            self.fast_next_expiry_ms = all
                .iter()
                .map(|p| p.expires_at_ms)
                .filter(|t| *t > now_ms)
                .min()
                .unwrap_or(i64::MAX);
        }
    }

    /// A market's resolution arrived from the venue (live) or was synthesized by
    /// the core (dry). Closes every position held on that market exactly once and
    /// books the payout as a receivable.
    ///
    /// Loud on anything it cannot settle: a position whose token the resolution
    /// does not price stays OPEN (inventing a payout would be worse than the
    /// drift it fixes) and the market stays watched.
    pub fn on_market_resolution(
        &mut self,
        resolution: blitzkrieg_market_api::MarketResolution,
        now_ms: i64,
    ) {
        self.settlement
            .note_answer(&resolution.condition_id, resolution.resolved, now_ms);
        if !resolution.resolved {
            return; // not resolved yet — the query re-arms on its own cadence
        }
        let mut market_positions: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.condition_id == resolution.condition_id)
            .cloned()
            .collect();
        // Paying legs first: the market's claim must exist before a worthless leg
        // joins it, because a NegRisk redemption takes the whole outcome-share
        // vector in one call (a leg booked first could not be added afterwards).
        market_positions.sort_by_key(|p| {
            std::cmp::Reverse(
                resolution
                    .payout_per_share(&p.token_id)
                    .unwrap_or(Decimal::ZERO),
            )
        });
        if market_positions.is_empty() {
            self.settlement.forget_market(&resolution.condition_id);
            return;
        }
        let mut settled = 0usize;
        let mut payout_usd = Decimal::ZERO;
        let mut unpriced: Vec<String> = Vec::new();
        for pos in &market_positions {
            let Some(booking) = booking_for(pos, &resolution) else {
                unpriced.push(format!("{} (position {})", pos.token_id, pos.id));
                continue;
            };
            // Commit BEFORE closing. The journal line is what makes the close
            // replay-safe (and `book` returns None for a key it already holds, so
            // a re-delivered resolution cannot book the same position twice).
            let Some(committed) = self.settlement.book(&booking, &resolution, now_ms) else {
                continue;
            };
            payout_usd += committed.payout_usd;
            // A redemption through the CTF pays no fee, so the settlement exit is
            // a maker close: `close` charges no exit fee and the realized net is
            // `payout − entry cost − entry fee` — exactly the movement the
            // identity's receivable records.
            let closed = self.positions.close(
                &committed.position_id,
                committed.payout_per_share,
                SETTLEMENT_EXIT_REASON,
                true,
                now_ms,
            );
            match closed {
                Some(closed) => {
                    // Trade record first, then the position snapshot: recovery
                    // needs the realized record to already exist, because the
                    // audit's expected side is built from it (see
                    // `SettlementBook::recover`).
                    self.on_position_closed(&closed, now_ms);
                    self.settlement.note_closed(&committed.key, now_ms);
                    self.persist_positions();
                    settled += 1;
                    tracing::info!(
                        position = %committed.position_id,
                        condition = %committed.condition_id,
                        shares = %committed.shares,
                        payout_per_share = %committed.payout_per_share,
                        payout_usd = %committed.payout_usd,
                        winning = committed.winning,
                        source = %resolution.source,
                        "position settled at market resolution"
                    );
                }
                None => {
                    // The booking is committed but the position was not in the
                    // book (an external close raced it). Nothing to close, and
                    // the claim stands: the tokens are ours on-chain whatever the
                    // local book says.
                    tracing::warn!(
                        position = %committed.position_id,
                        "settlement booked but the position was already closed locally"
                    );
                }
            }
        }
        if settled > 0 {
            self.persist_positions();
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "settled {settled} position(s) on {} — {payout_usd} USDC receivable pending redemption",
                    resolution.condition_id
                ),
            });
        }
        if !unpriced.is_empty() {
            let message = format!(
                "settlement: resolution of {} (source {}) prices no outcome for {} — left open",
                resolution.condition_id,
                resolution.source,
                unpriced.join(", ")
            );
            tracing::error!("{message}");
            // Through the one error slot as well: a market that cannot be priced
            // is why positions sit unresolved, and that must be visible in
            // `engine.stats` without reading logs (#180).
            self.emit_error(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::Internal,
                message,
            ));
            return; // keep the market watched: a later resolution may price it
        }
        self.settlement.forget_market(&resolution.condition_id);
    }

    /// Claims due for a redemption attempt. The plugin drains this in live mode
    /// and reports each one back through `on_redemption_result`; dispatching
    /// stamps the claim's backoff, so a result that never arrives is retried
    /// rather than forgotten.
    pub fn take_pending_redemptions(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        self.settlement.take_redemptions(now_ms)
    }

    /// #647: the per-venue redemption drain for the host's multi-plugin
    /// executor pulls. Each request carries its own venue (the claim's
    /// position-side execution-venue stamp), and this returns exactly the
    /// claims of `venue` — plus the UNTAGGED claims when `untagged_fallback`
    /// (the requesting plugin is the host's active one), matching the orders
    /// drain's single-plugin fallback. A sibling executor never sees a claim
    /// this drain handed out (the dispatch stamp is take-once).
    pub fn take_pending_redemptions_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        self.settlement
            .take_redemptions_for_venue(venue, untagged_fallback, now_ms)
    }

    /// A redemption attempt came back.
    ///
    /// Success is the exact instant the payout becomes cash: the claim leaves the
    /// receivable and the same amount enters the ledger balance, so the audit's
    /// `total` is unchanged by it. Failure is loud, is retried on the book's
    /// backoff, and never blocks a close or a new entry — this path touches
    /// neither the risk gate nor the order manager.
    pub fn on_redemption_result(
        &mut self,
        result: blitzkrieg_market_api::RedemptionResult,
        now_ms: i64,
    ) {
        match result.failure {
            None => {
                let Some((account_id, payout)) = self.settlement.note_confirmed(
                    &result.id,
                    result.tx_hash.clone(),
                    result.block_number,
                    now_ms,
                ) else {
                    // Unknown or already-redeemed claim: crediting here is how a
                    // phantom payout would enter the books, so it does not.
                    tracing::warn!(
                        claim = %result.id,
                        "redemption success for an unknown or already-redeemed claim — ignored"
                    );
                    return;
                };
                // §9.2: the payout lands in the CLAIM's own account — the claim
                // carried `account_id` from the position at booking time, the
                // same attribution the merge path already lives by. An empty
                // account (a settlement journal written before the field
                // existed) and an unknown account both fall back to the ACTIVE
                // book, the 0.3 behaviour, loudly: the cash has no address of
                // its own, so it goes where the process funder says — visible
                // in the log, never silently swallowed.
                if account_id.is_empty() {
                    self.accounts
                        .active_ledger_mut()
                        .credit_redemption(payout, &result.id);
                } else {
                    let a = AccountId::from(account_id.as_str());
                    match self.accounts.get_mut(&a) {
                        Ok(l) => l.credit_redemption(payout, &result.id),
                        Err(e) => {
                            tracing::warn!(
                                claim = %result.id,
                                account = %account_id,
                                error = %e,
                                "redemption claim names an unconfigured account — payout credited to the active book (0.3 fallback)"
                            );
                            self.accounts
                                .active_ledger_mut()
                                .credit_redemption(payout, &result.id);
                        }
                    }
                }
                tracing::info!(
                    claim = %result.id,
                    condition = %result.condition_id,
                    account = %account_id,
                    payout_usd = %payout,
                    tx = result.tx_hash.as_deref().unwrap_or(""),
                    block = result.block_number.unwrap_or_default(),
                    "settled claim redeemed on-chain — payout credited to cash"
                );
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::Internal,
                    message: format!(
                        "redeemed settled position: +{payout} USDC on-chain (claim {}, tx {})",
                        result.id,
                        result.tx_hash.as_deref().unwrap_or("n/a")
                    ),
                });
            }
            Some(failure) => {
                let what = if failure.manual {
                    "manual redemption required"
                } else {
                    "will retry"
                };
                let message = format!(
                    "redemption of claim {} failed ({what}): {}",
                    result.id, failure.message
                );
                if !self.settlement.note_failure(&result.id, &failure, now_ms) {
                    tracing::error!(
                        claim = %result.id,
                        error = %failure.message,
                        "redemption failed for an unknown or already-redeemed claim"
                    );
                } else {
                    tracing::error!(
                        claim = %result.id,
                        manual = failure.manual,
                        error = %failure.message,
                        "redemption failed"
                    );
                }
                // The reason went to the structured channel just above; the
                // stream write stays a static literal (#693 family — kernel
                // text at the stream sink is what CodeQL flags).
                eprintln!("core: redemption failed — see the error log line above");
                // Through the one slot as well: a claim whose collateral is stuck
                // is money the operator is waiting on (#180).
                self.emit_error(CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                ));
            }
        }
    }

    /// Claims due for a merge attempt. The plugin drains this in live mode and
    /// reports each one back through `on_merge_result`; dispatching stamps the
    /// claim's backoff, so a result that never arrives is retried rather than
    /// forgotten. Mirrors `take_pending_redemptions`.
    pub fn take_pending_merges(&mut self, now_ms: i64) -> Vec<blitzkrieg_market_api::MergeRequest> {
        self.settlement.take_merges(now_ms)
    }

    /// #647: the per-venue merge drain for the host's multi-plugin executor
    /// pulls. Routed exactly like
    /// [`Self::take_pending_redemptions_for_venue`] — own-venue claims, plus
    /// untagged claims when `untagged_fallback` — so a second venue's executor
    /// can never drain a foreign bridge's merge.
    pub fn take_pending_merges_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::MergeRequest> {
        self.settlement
            .take_merges_for_venue(venue, untagged_fallback, now_ms)
    }

    /// #461: a merge attempt came back. The SAME instant semantics as a
    /// redemption — confirmation is the exact moment the receivable becomes
    /// cash, on the account whose wallet held the burned pairs (the claim
    /// carries the account; E28 §9.2). An unknown claim's confirmation is
    /// ignored, never credited — crediting it is how a phantom payout enters
    /// the books.
    pub fn on_merge_result(&mut self, result: blitzkrieg_market_api::MergeResult, now_ms: i64) {
        match result.failure {
            None => {
                let Some((account_id, usd)) = self.settlement.note_merge_confirmed(
                    &result.id,
                    result.tx_hash.clone(),
                    result.block_number,
                    now_ms,
                ) else {
                    tracing::warn!(
                        claim = %result.id,
                        "merge success for an unknown or already-confirmed claim — ignored"
                    );
                    return;
                };
                match self.accounts.get_mut(&AccountId::from(account_id.as_str())) {
                    Ok(l) => {
                        l.credit_merge(usd, &result.id);
                        tracing::info!(
                            claim = %result.id,
                            condition = %result.condition_id,
                            account = %account_id,
                            usd = %usd,
                            tx = result.tx_hash.as_deref().unwrap_or(""),
                            "merge confirmed on-chain — collateral credited to cash"
                        );
                        self.emit(Event::RiskAlert {
                            code: blitzkrieg_market_api::CoreErrorCode::Internal,
                            message: format!(
                                "merge confirmed: +{usd} USDC on-chain (claim {}, tx {})",
                                result.id,
                                result.tx_hash.as_deref().unwrap_or("n/a")
                            ),
                        });
                    }
                    Err(e) => {
                        // The claim's cash has no book to land in — louder than a
                        // mint, and not silently swallowed: the receivable was
                        // already removed, so this IS the last chance to say.
                        tracing::error!(
                            account = %account_id,
                            error = %e,
                            "merge confirmation dropped: unknown account — collateral NOT credited"
                        );
                        self.emit_error(CoreError::new(
                            blitzkrieg_market_api::CoreErrorCode::Internal,
                            format!(
                                "merge {} confirmed {usd} USDC for unknown account {account_id} — cash NOT credited",
                                result.id
                            ),
                        ));
                    }
                }
            }
            Some(failure) => {
                let what = if failure.manual {
                    "manual merge required"
                } else {
                    "will retry"
                };
                let message = format!(
                    "merge of claim {} failed ({what}): {}",
                    result.id, failure.message
                );
                if !self
                    .settlement
                    .note_merge_failure(&result.id, &failure, now_ms)
                {
                    tracing::error!(
                        claim = %result.id,
                        error = %failure.message,
                        "merge failed for an unknown or already-confirmed claim"
                    );
                } else {
                    tracing::error!(
                        claim = %result.id,
                        manual = failure.manual,
                        error = %failure.message,
                        "merge failed"
                    );
                }
                // Static literal at the stream sink (#693 family); the reason
                // is in the structured error line just above.
                eprintln!("core: merge failed — see the error log line above");
                self.emit_error(CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                ));
            }
        }
    }

    /// Dry mode: simulate the redemption of every due claim. The claim is
    /// confirmed with a `dry-simulated` tx hash, so the ledger move is the same
    /// one a mined transaction produces and the whole path (settle → receivable →
    /// cash) is testable with no chain at all.
    ///
    /// `config.dry_redeem_fail` (test hook, off by default) makes the first N
    /// attempts of each claim fail instead. The failure travels the production
    /// path — `on_redemption_result` → `note_failure` → backoff, or a permanent
    /// stop when it is `manual` — so a gate can drive the retry clock and the
    /// receivable with no venue to fail for it. The count is per claim (its
    /// `attempts` counter, which the dispatch stamps), so the retry that follows
    /// a backoff is attempt 2 and lands.
    pub(super) fn simulate_redemptions(&mut self, now_ms: i64) {
        for request in self.settlement.take_redemptions(now_ms) {
            let attempts = self
                .settlement
                .claim(&request.id)
                .map(|c| c.attempts)
                .unwrap_or(0);
            let failure = (attempts <= self.config.dry_redeem_fail).then(|| {
                blitzkrieg_market_api::RedemptionFailure {
                    message: format!(
                        "dry-simulated redemption failure (attempt {attempts} of {} injected)",
                        self.config.dry_redeem_fail
                    ),
                    manual: self.config.dry_redeem_manual,
                }
            });
            let result = blitzkrieg_market_api::RedemptionResult {
                id: request.id.clone(),
                condition_id: request.condition_id.clone(),
                tx_hash: failure.is_none().then(|| "dry-simulated".to_string()),
                block_number: None,
                failure,
                at_ms: now_ms,
            };
            self.on_redemption_result(result, now_ms);
        }
    }

    /// Dry mode: simulate the merge confirmation of every due claim — the same
    /// shape as `simulate_redemptions`, so dry mode exercises the whole merge
    /// path (burn → receivable → cash) with no chain. In dry there is no venue
    /// to fail the merge, so the first dispatch always lands; `simulate_merges`
    /// is the only consumer of the claim's dispatch stamp, and a lost tick
    /// simply retries on the book's backoff.
    pub(super) fn simulate_merges(&mut self, now_ms: i64) {
        for request in self.settlement.take_merges(now_ms) {
            let result = blitzkrieg_market_api::MergeResult::success(
                &request,
                "dry-simulated",
                None,
                now_ms,
            );
            self.on_merge_result(result, now_ms);
        }
    }

    /// Answer a settlement query locally (dry/read-only modes). The rule: the
    /// token whose VALUE EVIDENCE is highest wins and pays 1, the others pay 0
    /// — and when no token's evidence clears 0.5 the resolution is WITHHELD
    /// (`None`): the query re-arms on its own cadence and the blind alert
    /// fires. A binary market past expiry always has a winner on-chain, so
    /// booking 0 for every token would silently zero a REAL winner — the
    /// bid-less-winner bug this ladder exists for. "Cannot tell" now holds the
    /// position open loudly instead of settling it at an invented 0.
    ///
    /// Evidence per token, first rung that has a price:
    ///   1. a two-sided book → its mid (the original rule, unchanged);
    ///   2. a bid-less book with a real ask → THE ASK: the buy side being
    ///      swept does not make the winner worthless. At expiry nobody sells a
    ///      sure dollar cheap, so the winner's ask sits near 1 while a loser's
    ///      sits near 0 — the old code read a one-sided mid (zeroed by
    ///      `from_levels`) and settled bid-less winners to 0;
    ///   3. an ask-less book with a bid → the bid (still a real level);
    ///   4. no usable book → the position's own last valuation.
    ///
    /// Documented as a simulation rule, not a market fact: `source` says
    /// `core-dry`, so a simulated settlement can never be read as a real one.
    /// It flows through `on_market_resolution` exactly like a venue answer —
    /// one booking path, two sources, both traceable (`core-dry` vs the
    /// venue's own).
    pub(super) fn dry_resolution(
        &self,
        query: &blitzkrieg_market_api::SettlementQuery,
    ) -> Option<blitzkrieg_market_api::MarketResolution> {
        let positions = self.positions.open_positions();
        let priced: Vec<(&str, Decimal)> = query
            .token_ids
            .iter()
            .map(|t| {
                let book = self.books.get(t);
                let top_bid = book
                    .map(|b| {
                        b.bids
                            .iter()
                            .map(|(p, _)| *p)
                            .filter(|p| *p > Decimal::ZERO)
                            .max()
                            .unwrap_or(Decimal::ZERO)
                    })
                    .unwrap_or(Decimal::ZERO);
                // A real ask is < 1; the empty-side sentinel `best_ask = 1`
                // must never read as "someone offers a dollar".
                let top_ask = book.and_then(|b| {
                    b.asks
                        .iter()
                        .map(|(p, _)| *p)
                        .filter(|p| *p > Decimal::ZERO && *p < Decimal::ONE)
                        .min()
                });
                let mut value = if top_bid > Decimal::ZERO {
                    match top_ask {
                        Some(a) => (top_bid + a) / Decimal::TWO,
                        None => top_bid,
                    }
                } else {
                    top_ask.unwrap_or(Decimal::ZERO)
                };
                if value <= Decimal::ZERO {
                    value = positions
                        .iter()
                        .find(|p| &p.token_id == t)
                        .map(|p| p.current_price)
                        .unwrap_or(Decimal::ZERO);
                }
                (t.as_str(), value)
            })
            .collect();
        if priced.is_empty() {
            return None;
        }
        let (winner, best) =
            priced.iter().fold(
                ("", Decimal::ZERO),
                |acc, (t, p)| {
                    if *p > acc.1 { (*t, *p) } else { acc }
                },
            );
        // Withhold instead of "pays nobody": at expiry the only honest
        // zero-payout is a named loser, and naming one requires evidence above
        // the coin-flip. Anything darker is our view being broken — re-arm.
        if best <= Decimal::new(5, 1) {
            // Counter-evidence ladder: our leg priced below the coin-flip AND
            // the condition's OTHER token decisively bid (≥ COUNTER_DECISIVE,
            // Gamma's own DECISIVE_HIGH). At expiry nobody pays 0.99 for a
            // coin still in the air — the partner's bid IS the venue pricing
            // the verdict, so our leg is the named loser and settles at 0.
            // Without this the leg withholds forever: the venue never answers
            // an expired round's query and the position blocks capacity until
            // an operator flattens it by hand (the ETH/BTC zombie positions).
            // The evidence key is the position's stamped partner token; a
            // position without one (old snapshots) keeps the old withhold.
            let partners: Vec<String> = positions
                .iter()
                .filter(|p| query.token_ids.contains(&p.token_id))
                .filter(|p| !p.partner_token_id.is_empty())
                .map(|p| p.partner_token_id.clone())
                .filter(|t| !query.token_ids.contains(t))
                .collect();
            let counter = partners
                .iter()
                .filter_map(|t| {
                    let book = self.books.get(t)?;
                    let top_bid = book
                        .bids
                        .iter()
                        .map(|(p, _)| *p)
                        .filter(|p| *p > Decimal::ZERO)
                        .max()?;
                    (top_bid >= COUNTER_DECISIVE_HIGH).then(|| (t.clone(), top_bid))
                })
                .max_by_key(|(_, bid)| *bid);
            if let Some((partner, _)) = counter {
                let payouts = priced
                    .iter()
                    .map(|(t, _)| ((*t).to_string(), Decimal::ZERO))
                    .chain(std::iter::once((partner, Decimal::ONE)))
                    .collect();
                return Some(blitzkrieg_market_api::MarketResolution {
                    condition_id: query.condition_id.clone(),
                    resolved: true,
                    payouts,
                    neg_risk: false,
                    source: "core-dry".to_string(),
                });
            }
            return None;
        }
        let payouts = priced
            .iter()
            .map(|(t, _)| {
                let payout = if *t == winner {
                    Decimal::ONE
                } else {
                    Decimal::ZERO
                };
                ((*t).to_string(), payout)
            })
            .collect();
        Some(blitzkrieg_market_api::MarketResolution {
            condition_id: query.condition_id.clone(),
            resolved: true,
            payouts,
            // A simulated market has no venue metadata to carry.
            neg_risk: false,
            source: "core-dry".to_string(),
        })
    }

    /// One settlement round, dry modes only: the core asks itself (through the
    /// same query path the venue plugin uses) and answers from the last books it
    /// saw. Live resolutions arrive from the venue instead.
    ///
    /// The simulated redemption runs at the end of the round, so dry mode
    /// exercises the whole path (settle → receivable → cash) with no chain —
    /// `on_market_resolution` itself only ever books, whatever the mode.
    pub(super) fn drive_settlement(&mut self, now_ms: i64) {
        if !self.config.mode.settles_locally() {
            return;
        }
        for query in self.take_settlement_queries(now_ms) {
            if let Some(resolution) = self.dry_resolution(&query) {
                self.on_market_resolution(resolution, now_ms);
            }
        }
        self.simulate_redemptions(now_ms);
        self.simulate_merges(now_ms);
    }

    /// Alert once when a watched market stops being answered (see
    /// [`SettlementBook::note_blind_state`]). Loud but not repeated: the panel's
    /// `blindSinceMs` is the continuous signal.
    pub(super) fn alert_if_settlement_blind(&mut self, now_ms: i64) {
        let blind = self.settlement.blind_since_ms(now_ms);
        if self.settlement.note_blind_state(blind.is_some()) {
            let since = blind.unwrap_or(now_ms);
            let message = format!(
                "settlement: no market verdict since {since} — {} market(s) past expiry cannot settle until the venue answers",
                self.settlement.tracked_markets()
            );
            tracing::error!("{message}");
            // Also the one slot (#180): "settlement has been blind since X" is the
            // single most important thing an operator can know about a live book.
            self.emit_error(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::Internal,
                message,
            ));
        }
    }

    /// #684: close positions the settlement path can never resolve — a market
    /// whose expiry is long past AND whose verdict query has never been
    /// answered by the venue. The updown round list only carries future rounds,
    /// so an already-expired condition never appears in an answer: waiting is
    /// not patience, it is a ghost position forever (`positions.exit` bounces
    /// off the venue's "Insufficient position", and nothing else can clear it).
    ///
    /// The close is deliberately NOT a settlement:
    ///   * price = the position's last known mid; `0` (a dead feed) falls back
    ///     to the entry price, i.e. cost recovery — never an invented payout;
    ///   * NO redemption claim is booked (`SettlementBook::book` is not
    ///     called): with no verified verdict, redeeming a "winner" would be
    ///     redeeming an invention. The venue's own cash truth folds in through
    ///     the existing reconcile/realign sweep, which is exactly why this
    ///     price may stay conservative;
    ///   * the trade record's exit reason is `unverifiable_expiry`
    ///     ([`UNVERIFIED_EXPIRY_EXIT_REASON`]), so history can never be read
    ///     as a venue verdict.
    ///
    /// Idempotent by construction: the position leaves the open book on the
    /// first close, so the second pass finds nothing; the watch list forgets
    /// the market through [`Self::sync_settlement_watch`] once its last
    /// position is gone.
    pub(super) fn run_expiry_fallback_sweep(&mut self, now_ms: i64) {
        // Only markets the venue has structurally never answered, past the
        // silence bound. `answers > 0` means the venue is ruling on this
        // condition honestly (even "not resolved yet") — keep waiting.
        let unanswered: std::collections::HashSet<String> = self
            .settlement
            .unanswered_since(EXPIRY_FALLBACK_AFTER_MS, now_ms)
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        if unanswered.is_empty() {
            return;
        }
        let targets: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| {
                unanswered.contains(&p.condition_id)
                    // Only past-expiry holdings qualify: a future expiry with an
                    // unanswered query is still (theoretically) settleable.
                    && p.expires_at_ms <= now_ms
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        let closed_count = targets.len();
        for pos in targets {
            // Fallback price: the last known mid; a zeroed feed recovers cost
            // via the entry price. Both are bookkeeping values — the venue's
            // real cash position is what the reconcile sweep reports.
            let price = if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                pos.entry_price
            };
            let closed =
                self.positions
                    .close(&pos.id, price, UNVERIFIED_EXPIRY_EXIT_REASON, true, now_ms);
            if let Some(closed) = closed {
                // The position left the open book, so the audit identity's
                // spent/received folds shrink by its flows. The settlement path
                // covers that with a receivable claim; this path books none, so
                // the fallback notional moves into cash HERE — exactly what
                // `close` priced the un-sold shares at (fee-free maker close).
                self.credit_expiry_fallback_cash(&closed, price);
                // Trade record first, then the snapshot — the same order the
                // settlement path closes with (the audit reads the record).
                self.on_position_closed(&closed, now_ms);
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    account = %closed.account_id.as_str(),
                    shares = %closed.shares,
                    fallback_price = %price,
                    "position closed at unverifiable expiry — the venue never answered this market's settlement query; \
                     no redemption claim booked (cash truth follows the reconcile sweep)"
                );
            }
        }
        self.persist_positions();
        // A market whose LAST position left via the fallback: the watch would
        // only forget it on the next drain's `sync_settlement_watch` — forget
        // it NOW (the same thing `on_market_resolution` does), so the panel's
        // `trackedMarkets` reads 0 immediately and `unanswered_since` stops
        // naming a market we no longer hold.
        for condition_id in &unanswered {
            if self
                .positions
                .open_positions()
                .iter()
                .all(|p| p.condition_id != *condition_id)
            {
                self.settlement.forget_market(condition_id);
            }
        }
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Internal,
            message: format!(
                "expiry fallback: closed {} position(s) on market(s) the venue never answered \
                 (unverifiable-expiry, no redemption claim — reconcile sweep owns the cash truth)",
                closed_count
            ),
        });
    }

    /// #684 shared ledger leg of the fallback close: credit what `close` priced
    /// the position's un-sold shares at (`price × dust_shares`; a maker close
    /// charges no exit fee) into the POSITION's own account book. Without it the
    /// position walks out of the open book with no compensating entry and
    /// `trade_ledger_identity` drifts by exactly the closed notional. The
    /// journal row's kind is `ExpiryFallback` — never a venue sell, never a
    /// redemption: the venue's truth stays the reconcile sweep's to report.
    /// An unknown account cannot happen (the position was opened in a
    /// configured book), so a failure is logged loudly rather than folded into
    /// the active book's cash.
    pub(super) fn credit_expiry_fallback_cash(
        &mut self,
        closed: &crate::position::ClosedPosition,
        price: Decimal,
    ) {
        let amount = price * closed.dust_shares;
        if amount <= Decimal::ZERO {
            return;
        }
        let account = closed.account_id.clone();
        let position_id = closed.id.clone();
        if let Err(e) = self
            .accounts
            .get_mut(&account)
            .map(|l| l.credit_expiry_fallback(amount, &position_id))
        {
            tracing::error!(
                position = %position_id,
                account = %account.as_str(),
                error = %e,
                "expiry fallback credit FAILED — cash identity will drift by {amount}"
            );
        }
    }

    /// #684: the operator's clear-out verb for one EXPIRED market's position
    /// (`positions.force_close`). The kernel-side gates, in order:
    ///   1. the position exists (unknown id → refused, nothing written);
    ///   2. it is actually past its expiry — a live round is refused with a
    ///      pointer to `positions.exit`, because a position the venue can still
    ///      settle (or an order that can still fill) is not a ghost;
    ///   3. a manual-ops audit path is configured — same gate
    ///      `system.reset_trading` sits behind: an unaudited manual close of a
    ///      real-money record must fail rather than happen silently.
    ///
    /// The close itself rides the SAME fallback shape as
    /// [`Self::run_expiry_fallback_sweep`]: last known mid, else entry price;
    /// NO redemption claim (`SettlementBook::book` is never called — the
    /// verdict is unverified, so no payout is claimed and no winner redeemed;
    /// the venue's cash truth follows the reconcile/realign sweep). The
    /// difference is provenance: this path appends a manual-ops audit line
    /// naming the operator, and the trade record reads `unverifiable_expiry`
    /// either way.
    ///
    /// Concurrency note: `&mut self` is the whole story — the Service is the
    /// lock; no second mutex is introduced (#685 standing rule).
    pub fn force_close_expired_position(
        &mut self,
        position_id: &str,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::position::ClosedPosition> {
        let pos = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.id == position_id)
            .cloned()
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("positions.force_close: no open position \"{position_id}\""),
                )
            })?;
        // Expiry by SLOT MATCHING, mirroring the order-side force_close_order:
        // slot N covers [N·dur, (N+1)·dur) on the position's OWN family clock,
        // so the round is over once now passes the slot's end. `0` = legacy
        // snapshot without a slot fact — fall back to the expires_at_ms
        // countdown the open path anchored to (slot+1)·dur anyway.
        let expired = if pos.round_slot > 0 {
            let eff = pos.effective_round_duration_sec(None, self.config.round_duration_sec);
            now_ms >= (pos.round_slot + 1) * eff * 1000
        } else {
            now_ms > pos.expires_at_ms
        };
        if !expired {
            return Err(CoreError::new(
                CoreErrorCode::InvalidOrderState,
                format!(
                    "positions.force_close: position {position_id}'s round is still live (slot {}, \
                     expires_at {}) — use positions.exit for a position the venue can still settle",
                    pos.round_slot, pos.expires_at_ms
                ),
            ));
        }
        let audit_path = self
            .config
            .manual_ops_audit_path
            .clone()
            .map(std::path::PathBuf::from);
        let Some(audit_path) = audit_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "positions.force_close: no manual-ops audit path configured — \
                 an operator close with no audit trail is refused (backtests and \
                 in-process tests carry None)"
                    .to_string(),
            ));
        };
        // The same fallback price the sweep uses: last known mid, else entry
        // (cost recovery). Bookkeeping value, never an invented payout.
        let price = if pos.current_price > Decimal::ZERO {
            pos.current_price
        } else {
            pos.entry_price
        };
        let closed = self
            .positions
            .close(
                position_id,
                price,
                UNVERIFIED_EXPIRY_EXIT_REASON,
                true,
                now_ms,
            )
            .ok_or_else(|| {
                // Lost a race with another closer: refuse without writing
                // anything (no audit line for a close that did not happen).
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("positions.force_close: no open position \"{position_id}\""),
                )
            })?;
        // Trade record first, then the snapshot — the close paths' shared order.
        self.credit_expiry_fallback_cash(&closed, price);
        self.on_position_closed(&closed, now_ms);
        self.persist_positions();
        // The manual-ops audit row is the operator's fingerprint on the close:
        // who, what, at which price, on which market. `append_manual_op` (the
        // reset path's writer) fails loudly — surfaced as a refusal-shaped
        // error, though the close itself has landed by then; the warn names it.
        if let Err(e) = crate::ipc::runtime_config::append_manual_op(
            std::path::Path::new(audit_path),
            &serde_json::json!({
                "op": "positions.force_close",
                "atMs": now_ms,
                "actor": actor,
                "positionId": closed.id,
                "conditionId": closed.condition_id,
                "tokenId": closed.token_id,
                "accountId": closed.account_id.as_str(),
                "strategy": closed.strategy,
                "shares": closed.shares.to_string(),
                "price": price.to_string(),
                "exitReason": "unverifiable_expiry",
            }),
        ) {
            tracing::error!(
                position = %closed.id,
                error = %e,
                "positions.force_close: manual-ops audit write FAILED — the close landed, the audit row did not"
            );
        }
        tracing::warn!(
            position = %closed.id,
            condition = %closed.condition_id,
            actor = %actor,
            price = %price,
            "position force-closed by operator (unverifiable-expiry, no redemption claim)"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Internal,
            message: format!(
                "operator force-closed position {} on {} at {price} (unverifiable-expiry; \
                 no redemption claim — reconcile sweep owns the cash truth)",
                closed.id, closed.condition_id
            ),
        });
        Ok(closed)
    }

    /// Finish settlements whose close never became durable (a crash between the
    /// journal commit and the close). The booking stands — the tokens are ours
    /// on-chain — so the position is closed here exactly once, at the recorded
    /// payout price.
    ///
    /// Called from [`Core::restore_positions`], before trading resumes: the
    /// trade record has to exist for the audit's expected side to balance (see
    /// `SettlementBook::recover`), and this is the only moment the books can
    /// still be repaired.
    pub(super) fn recover_settlements(&mut self) -> usize {
        let stale: Vec<crate::position::OpenPosition> = self
            .settlement
            .recover(self.positions.open_positions())
            .into_iter()
            .cloned()
            .collect();
        let mut recovered = 0usize;
        for pos in stale {
            let key = settlement_key(&pos);
            let Some(record) = self.settlement.record(&key) else {
                continue;
            };
            let (payout_per_share, at_ms) = (record.payout_per_share, record.applied_at_ms);
            let closed = self.positions.close(
                &pos.id,
                payout_per_share,
                SETTLEMENT_EXIT_REASON,
                true,
                at_ms,
            );
            if let Some(closed) = closed {
                // The trade record was lost with the crash: write it now, once,
                // and mark the close durable so no later start repeats it.
                self.on_position_closed(&closed, at_ms);
                self.settlement.note_closed(&key, at_ms);
                recovered += 1;
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    payout_per_share = %payout_per_share,
                    "recovered a settlement whose close never landed — closed at the booked payout"
                );
            }
        }
        if recovered > 0 {
            self.persist_positions();
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "recovered {recovered} settled position(s) whose close was interrupted — redeemed claims unaffected"
                ),
            });
        }
        recovered
    }

    /// The settlement block of the engine stats: what is settled, what the chain
    /// still owes us, and anything stuck. Read by the panel through the existing
    /// `engine_stats` outlet (no new event type).
    pub(super) fn settlement_view(&self, as_of_ms: i64) -> serde_json::Value {
        let claims: Vec<serde_json::Value> = self
            .settlement
            .claims()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "conditionId": c.condition_id,
                    "payoutUsd": dec_json(c.payout_usd),
                    "negRisk": c.neg_risk,
                    "shares": c.outcome_shares.iter().map(|s| dec_json(*s)).collect::<Vec<_>>(),
                    "positions": c.position_ids.len(),
                    "attempts": c.attempts,
                    "nextAttemptMs": c.next_attempt_ms,
                    "manual": c.manual,
                    "txHash": c.tx_hash,
                    "blockNumber": c.block_number,
                    "lastError": c.last_error,
                })
            })
            .collect();
        serde_json::json!({
            "settledPositions": self.settlement.applied_count(),
            "bookedThisSession": self.settlement.booked_this_session(),
            "receivableUsd": dec_json(self.settlement.receivable_usd()),
            "pendingRedemptions": self.settlement.pending_claim_count(),
            "retryableRedemptions": self.settlement.retryable_claim_count(),
            "manualRedemptions": self.settlement.manual_claim_count(),
            "redeemedClaims": self.settlement.redeemed_count(),
            "trackedMarkets": self.settlement.tracked_markets(),
            "blindSinceMs": self.settlement.blind_since_ms(as_of_ms),
            // #461: burned-pair collateral waiting on its merge receipt —
            // split from the redemption receivables so the panel can tell
            // "settled, waiting to redeem" from "merged, waiting to mine".
            "mergeReceivableUsd": dec_json(self.settlement.merge_receivable_usd()),
            "pendingMerges": self.settlement.pending_merge_count(),
            "lastError": match self.settlement.last_error() {
                Some((claim, message, manual)) => serde_json::json!({
                    "claim": claim, "message": message, "manual": manual,
                }),
                None => serde_json::Value::Null,
            },
            "claims": claims,
        })
    }
}
