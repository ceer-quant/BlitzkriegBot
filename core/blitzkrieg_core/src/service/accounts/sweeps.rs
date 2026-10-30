//! Expiry, pair-orphan and force-close sweeps (#780).

use crate::ipc::schema::Event;
use crate::model::*;
use rust_decimal::Decimal;

use super::Core;

impl Core {
    // Replay sched is excluded wholesale: the sweep mutates the order set (a
    // tradesSha256 input) and replayed archives have no ghost-relevant clock —
    // the historical per-order flows own every transition there.
    pub(crate) fn run_expiry_sweep(&mut self, now_ms: i64) {
        if self.replay_sched() {
            return;
        }
        let dur_ms = self.config.round_duration_sec * 1000;
        if dur_ms <= 0 {
            return;
        }
        // B1b: EVERY live order is judged on ITS OWN family clock — the
        // three-state effective duration (own stamp > market declaration >
        // global grid; a `0` family folds to the global grid, exactly the
        // pre-B1b value). Single-family equivalence: a deployment where every
        // market declares the configured grid (or nothing) and every stamp
        // rides the grid resolves eff == cfg here, byte-for-byte the old
        // filter. The expired ids are collected BEFORE the mutation pass (the
        // market lookup reads the engine).
        let mut expired: Vec<(String, i64)> = Vec::new();
        for o in self.ome.live_orders() {
            if o.round_slot <= 0 {
                continue;
            }
            let eff = self.order_eff_round_sec(o);
            if eff > 0 && now_ms >= (o.round_slot + 1) * eff * 1000 {
                expired.push((o.order_id.clone(), eff));
            }
        }
        for (id, eff) in expired {
            // Segment 2 of the wiring template: the lazy idempotent stamp —
            // a `0` family is written once from the resolution the expiry
            // judgment just made; a non-0 stamp is never touched.
            self.ome.backfill_round_duration(&id, eff);
            self.force_close_order(&id, now_ms, "round expired").ok();
        }
        // B1b lazy backfill: positions opened before the family stamp existed
        // (restored legacy snapshots) get their family written the first time
        // the expiry consumer touches them — idempotent
        // (`backfill_round_duration` never overwrites a non-0 stamp), so
        // running this every sweep is free. The families are resolved BEFORE
        // the mutable borrow (the market lookup reads the engine).
        let families: std::collections::HashMap<String, i64> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.round_duration_sec == 0)
            .map(|p| (p.token_id.clone(), self.round_family_of_token(&p.token_id)))
            .collect();
        self.positions
            .backfill_round_families(|tok| families.get(tok).copied().unwrap_or(0));
    }

    /// E-VA companion: the single-leg orphan sweep for ATOMIC pair strategies.
    ///
    /// The atomic batch (E-VA) guarantees pair legs leave the book together —
    /// but only at SUBMISSION. Afterwards the legs live and die on their own:
    /// one leg fills, the partner's resting order is cancelled (manual, the
    /// #457 expiry sweep, a venue reject) and the strategy is left holding a
    /// NAKED leg — exposure the pair economics never priced, earning nothing
    /// and settling at the coin flip. This sweep is the compensation watch:
    /// for every (account, strategy) scope that BOTH declared `atomic_batch`
    /// AND has a governed virtual pot, a position whose partner token has no
    /// sibling position, no live partner BUY order and no in-flight partner
    /// taker past the grace window is the orphan — the sweep first tries to
    /// CHASE the missing cross-venue leg inside the pair's budget (below),
    /// and only when that is refused — the book too dear, a guard or a
    /// placement refusal — alerts once (the existing `RiskAlert` event, no
    /// new wire type) and flattens through the manual exit path (`flatten`,
    /// Taker walk out of the position's own account).
    ///
    /// Scope discipline:
    ///   - zero-config = zero behaviour: a scope with no cap in
    ///     `virtual_limits` is ungoverned and never touched — the replay
    ///     baseline stays byte-identical;
    ///   - live path only (`replay_sched()` skips — the sweep is live-panel
    ///     semantics, the nofast replay loop keeps its own shape);
    ///   - the 5 s grace window covers the serial same-tick leg landing (both
    ///     legs of one batch fill inside one evaluate cycle, and the partner's
    ///     position row can lag the first leg's by that whole pass); a
    ///     constant, not a tunable — the window is a bookkeeping race bound,
    ///     not a policy dial;
    ///   - debounce is structural: a flattened leg leaves the book, so the
    ///     next tick's condition is simply false; a flatten that failed
    ///     because a live SELL already rests for the token is skipped by
    ///     `flatten` itself (no retry storm, no extra ledger).
    ///
    /// Chase compensation (#531, the feasible arm): the missing leg is the
    /// strategy's most recent BUY order on this account for a token that is
    /// NEITHER the naked leg NOR its same-market partner (the cross-venue
    /// counterpart — `partner_token_id` names only the same-market completion
    /// token, and OME history is the only place the dead leg's token, venue
    /// provenance and round identity survive). The budget is the pair's own:
    /// the naked leg's per-share basis (`cost_usd / shares`) plus the chase
    /// price must stay within the $1.00 a UP+DOWN share pair redeems — this
    /// is the same arithmetic `escalation_guard`'s pair-cost cap applies,
    /// but that cap is keyed by `condition_id` and cannot see a cross-venue
    /// counterpart whose legs carry different condition ids, so the sweep
    /// prices the budget ITSELF into the chase limit and hands the guard a
    /// request that cannot trip the cap. A stale or empty book (`None`
    /// walk) and any guard/placement refusal are the INFEASIBLE arm: the
    /// existing alert+flatten fallback below fires unchanged.
    pub(crate) fn run_pair_orphan_sweep(&mut self, now_ms: i64) {
        if self.replay_sched() {
            return;
        }
        /// Same-tick serial landing window for a pair's two legs (see the
        /// scope discipline above). Not a tunable.
        const PAIR_LAND_GRACE_MS: i64 = 5_000;
        /// How long a placed chase has to land before the sweep stops giving
        /// it the benefit of the doubt and falls back to alert+flatten. A
        /// dry taker fills inside the same `place` call (zero latency dials)
        /// so the window is really only a LIVE-path patience bound and a
        /// debounce: while it runs, the orphan condition below keeps being
        /// true (the partner still has no position/order) and without the
        /// window the sweep would flatten the leg out from under its own
        /// chase on the very next tick. A constant, not a tunable — the
        /// flatten fallback stays the final answer either way.
        const PAIR_CHASE_WINDOW_MS: i64 = 2_000;
        // The governed atomic scopes: every (account, strategy) pair the pot
        // book has a cap for — the strategy's `atomic_batch` declaration is
        // implied by the pot existing (`virtual_limits` is only wired to
        // atomic strategies in practice; a governed scope for a non-atomic
        // strategy is a config decision we honor the same way, since the
        // orphan condition below is strategy-shape agnostic).
        let governed: Vec<(String, String)> = self.virtual_accounts.scopes();
        for (account_id, strategy) in governed {
            // The live BUY orders this strategy rests on this account, keyed
            // by token: a partner with a still-breathing entry order is not
            // orphaned (its leg is arriving), and the position side of the
            // SAME order set covers the inverse asymmetry (first leg still
            // resting + second leg already held).
            let live_partner_buys: std::collections::HashSet<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| {
                    o.side == Side::Buy
                        && o.strategy == strategy
                        && o.account_id.as_str() == account_id.as_str()
                })
                .map(|o| o.token_id.clone())
                .collect();
            let inflight_tokens: std::collections::HashSet<String> = self
                .inflight_takers
                .iter()
                .filter_map(|leg| self.ome.get(&leg.order_id))
                .filter(|o| {
                    o.side == Side::Buy
                        && o.strategy == strategy
                        && o.account_id.as_str() == account_id.as_str()
                })
                .map(|o| o.token_id.clone())
                .collect();
            let orphans: Vec<(String, String, String, String, Decimal, Decimal)> = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| {
                    p.strategy == strategy
                        && p.account_id.as_str() == account_id
                        && !p.partner_token_id.is_empty()
                        // #779: the chase budget prices the per-share basis as
                        // `cost_usd / shares`. A zero-share row (an entry
                        // rolled back to zero, or a shell `drop_if_empty` has
                        // not removed yet) is not a naked leg — dividing by it
                        // PANICS. Never a candidate.
                        && p.shares > Decimal::ZERO
                        // Settlement-dead positions belong to the settlement
                        // ladder, not the unwind desk: a hold-to-settlement
                        // pair rides PAST its round end by design (the same
                        // live-only rule `can_open` and the systemic facts
                        // count by). A naked leg is only unwound at market
                        // prices while its market is still live.
                        && p.expires_at_ms > now_ms
                        && now_ms - p.entered_at_ms >= PAIR_LAND_GRACE_MS
                        && !self.positions.open_positions().iter().any(|sib| {
                            sib.token_id == p.partner_token_id
                                && sib.strategy == strategy
                                && sib.account_id.as_str() == account_id
                        })
                        // The #427 cross-venue completion set: the package's
                        // pair lands as OPPOSITE directions on the same asset
                        // in the same round (one leg per venue), while
                        // `partner_token_id` only ever names the SAME-market
                        // completion token — without this clause a fully
                        // landed pair would read as two orphans and be
                        // flattened whole, destroying the locked profit the
                        // watch exists to protect. An opposite-direction
                        // sibling in the same scope and round window (the
                        // shared `expires_at_ms` is the round identity the
                        // position carries) is the market-neutral set the
                        // strategy built; it is not naked.
                        && !self.positions.open_positions().iter().any(|sib| {
                            sib.strategy == strategy
                                && sib.account_id.as_str() == account_id
                                && sib.asset == p.asset
                                && sib.expires_at_ms == p.expires_at_ms
                                && sib.direction != p.direction
                        })
                        && !live_partner_buys.contains(&p.partner_token_id)
                        && !inflight_tokens.contains(&p.partner_token_id)
                })
                .map(|p| {
                    (
                        p.id.clone(),
                        p.token_id.clone(),
                        p.venue.clone(),
                        p.partner_token_id.clone(),
                        // The chase budget's paid half: the naked leg's
                        // per-share basis. The entry notional spent per
                        // share is what the missing leg's price must fit
                        // under ($1.00 a pair redeems).
                        p.cost_usd / p.shares,
                        p.shares,
                    )
                })
                .collect();
            for (id, token, venue, partner, basis, shares) in orphans {
                // ── Chase arm (#531 feasible): find the dead cross-venue leg
                // in OME history and re-offer it within the pair budget. The
                // infeasible arm keeps the historical alert+flatten shape
                // byte-for-byte.
                let account = AccountId::from(account_id.as_str());
                let chase = self.pair_chase_target(&account, &strategy, &token, &partner);
                let chased = match chase
                    .map(|t| {
                        // The budget cap the chase may state: the naked leg's
                        // basis is the paid half; the missing leg buys at the
                        // counterparty and the PAIR must stay inside $1.00.
                        (t, Decimal::ONE - basis)
                    })
                    .filter(|(_, cap)| *cap > Decimal::ZERO)
                {
                    Some((target, cap)) => self.pair_chase_missing_leg(
                        &target,
                        (&token, &account, &strategy),
                        shares,
                        cap,
                        now_ms,
                    ),
                    None => false,
                };
                if chased {
                    // The chase is out (a filled position or a resting BUY).
                    // The naked leg is not orphaned any more — the next tick's
                    // condition either reads the partner side (filled) or the
                    // live-BUY clause (resting), and the chase window above
                    // keeps this tick's re-judgment off the leg entirely.
                    continue;
                }
                // Chase window debounce: a chase this scope placed within the
                // window is still adjudicating (a live taker, a resting leg).
                // Skip WITHOUT alerting or flattening — the window is the
                // chase's benefit of the doubt; after it, the fallback below
                // fires.
                if let Some((_, at)) = self
                    .pair_chase_attempts
                    .get(&(account_id.clone(), strategy.clone(), token.clone()))
                    .cloned()
                    && now_ms - at < PAIR_CHASE_WINDOW_MS
                {
                    continue;
                }
                let message = format!(
                    "pair leg orphaned: strategy={strategy} account={account_id} \
                     held={token}@{venue} partner_missing={partner} — \
                     flattening the naked leg"
                );
                tracing::warn!(position = %id, token = %token, partner = %partner,
                    "pair leg orphaned — flattening the naked leg");
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::RiskRejected,
                    message,
                });
                // The manual exit path: SELL Taker walk out of the position's
                // own account (E28 §9.2), ExitReason::Manual — no new enum
                // value, the audit line above carries the real cause. A
                // refusal (a live SELL already resting, a halted breaker) is
                // the sweep's own skip posture: the position stays in the
                // book and the next tick re-judges it, so the result is
                // honestly dropped rather than swallowed.
                let _ = self.flatten(Some(&id), now_ms);
                // RFC 568 阶段一（观察者写点）：flatten 已处置 ⇒ 配对落
                // Closed 终态（判定链零改动 —— sweep 的 orphan 判定照旧直读
                // OME 与持仓，这里只落账）。寻址按 token 形（close_by_token）：
                // 经济键第二元是实例主账户，sweep 手里只有腿的落账账户
                // （v2 A≠B 时两者不同），token 唯一对应本轮一个市场，绕开
                // 这层不对称；幂等（同 token 跨轮旧键也一并闭账）。
                self.pair_book.close_by_token(&strategy, &token);
                // The chase did not save the leg: retire its attempt record so
                // the next tick can offer a fresh chase (a repriced book may
                // have made the pair feasible again) instead of waiting out a
                // stale window.
                self.pair_chase_attempts.remove(&(
                    account_id.clone(),
                    strategy.clone(),
                    token.clone(),
                ));
            }
        }
    }

    /// The chase arm's TARGET: the missing cross-venue leg's token, read out
    /// of the OME's order history. It is this (account, strategy) scope's
    /// most recent BUY for a token that is neither the naked leg nor the
    /// naked leg's same-market partner (`partner_token_id` names only the
    /// same-market completion token; the cross-venue counterpart's token is
    /// nowhere on the position row — the dead ORDER is the only evidence of
    /// which token the pair attempt was completing with). `None` = no
    /// missing-leg order ever existed (the entry died before resting — the
    /// starved-ledger shape) → the sweep goes straight to alert+flatten.
    pub(super) fn pair_chase_target(
        &self,
        account_id: &AccountId,
        strategy: &str,
        naked_token: &str,
        partner: &str,
    ) -> Option<String> {
        // The most recent attempt is the leg the strategy was completing:
        // `by_submitted` is the insertion order, so `.all()` is oldest first
        // — `.rfind()` reads the newest.
        self.ome
            .all()
            .into_iter()
            .filter(|o| {
                o.side == Side::Buy
                    && o.token_id != naked_token
                    && o.token_id != partner
                    && o.strategy == strategy
                    && o.account_id == *account_id
            })
            .rfind(|_| true)
            .map(|o| o.token_id.clone())
    }

    /// Re-offer the missing leg: price it off the CURRENT book (worst fill
    /// price the walk needs, `cap` = the pair budget's chase half), state the
    /// full naked size as a dry-FOK Taker, run the same guard every
    /// escalation runs, and place through `place_escalated` (entry gates off —
    /// this COMPLETES the naked position's pair, exactly the gap-fill case
    /// the one-per-asset gate would otherwise refuse). `Ok(true)` = the leg
    /// is out (filled into a position or resting live); `Ok(false)` or any
    /// error = infeasible, the caller's alert+flatten fallback proceeds.
    ///
    /// The internal key carries the slot so a NEXT round's orphan gets a
    /// fresh key (the OME refuses a duplicate live intent key): the round
    /// identity comes from the historical order, the natural fingerprint of
    /// "this pair attempt".
    /// The naked leg's identity, borrowed: (token, account, strategy) — the
    /// one tuple every chase bookkeeping (request provenance, attempt key,
    /// tracing) reads.
    pub(super) fn pair_chase_missing_leg(
        &mut self,
        missing_token: &str,
        naked: (&str, &AccountId, &str),
        size: Decimal,
        cap: Decimal,
        now_ms: i64,
    ) -> bool {
        let (naked_token, account_id, strategy) = naked;
        let Some((vwap, worst)) = self.marketable_walk(Side::Buy, missing_token, size, cap, now_ms)
        else {
            // The book cannot cover the naked size within the pair budget (or
            // is stale/absent) — the infeasible arm.
            return false;
        };
        // B1b: the chase completes the NAKED leg's pair attempt, so it runs on
        // the naked leg's family clock, not the configured grid — the missing
        // leg is sized/timed against the same round the pair was admitted
        // under. The naked POSITION carries the family (stamped at open,
        // backfilled at the expiry consumer); a `0` stamp folds to the global
        // grid, the pre-B1b slot fallback byte-for-byte.
        let naked_family = self
            .positions
            .open_positions()
            .iter()
            .find(|p| {
                p.token_id == naked_token
                    && p.account_id.as_str() == account_id.as_str()
                    && p.strategy == strategy
            })
            .map(|p| self.position_eff_round_sec(p))
            .filter(|eff| *eff > 0);
        let slot = self.books_round_slot(missing_token).unwrap_or_else(|| {
            let fam = naked_family
                .unwrap_or(self.config.round_duration_sec)
                .max(1);
            now_ms / (fam * 1000).max(1)
        });
        let internal_key = format!("pair-chase:{naked_token}:{missing_token}:{slot}");
        let req = OrderRequest {
            token_id: missing_token.to_string(),
            // The guard's pair-cost cap keys on condition_id and the
            // counterpart legs carry DIFFERENT condition ids (that is why the
            // sweep prices the budget itself); the request carries the naked
            // leg's identity so the accounting and events name the pair
            // attempt, not a fresh condition.
            condition_id: naked_token.to_string(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: worst,
            size,
            internal_key: internal_key.clone(),
            strategy: strategy.to_string(),
            asset: self
                .books_asset(missing_token)
                .unwrap_or_else(|| "BTC".to_string()),
            direction: self
                .books_direction(missing_token)
                .unwrap_or_else(|| "up".to_string()),
            round_slot: slot,
            account_id: account_id.clone(),
        };
        // One attempt per window: stamp BEFORE placing — if the place dies,
        // the stamp is what keeps this tick's fallback and the window's
        // re-judgment from double-firing.
        self.pair_chase_attempts.insert(
            (
                account_id.as_str().to_string(),
                strategy.to_string(),
                naked_token.to_string(),
            ),
            (missing_token.to_string(), now_ms),
        );
        if let Err(reason) = self.escalation_guard(&req, vwap) {
            tracing::info!(
                strategy = %strategy,
                account = %account_id.as_str(),
                token = %missing_token,
                naked = %naked_token,
                price = %worst,
                cap = %cap,
                reason = %reason,
                "pair chase refused by the guard — falling back to alert+flatten"
            );
            return false;
        }
        match self.place_escalated(req, now_ms) {
            Ok((order_id, status)) => {
                // B1b: the chase order inherits the naked leg's family — the
                // same lazy idempotent stamp the placement path's market
                // lookup would have resolved, but the PAIR attempt's family
                // is authoritative (the missing leg's own market may declare
                // a different family than the round the pair was priced
                // under). A non-0 stamp is never overwritten.
                if let Some(fam) = naked_family {
                    self.ome.backfill_round_duration(&order_id, fam);
                }
                tracing::info!(
                    order = %order_id,
                    ?status,
                    strategy = %strategy,
                    account = %account_id.as_str(),
                    token = %missing_token,
                    naked = %naked_token,
                    price = %worst,
                    size = %size,
                    "pair chase placed — the missing leg is re-offered past the counterparty"
                );
                // RFC 568 阶段一（观察者写点）：chase 成功 ⇒ 把 chase 订单的
                // internal_key 绑到配对簿的缺失腿（naked token 的配对腿）——
                // chase 的成交/死亡事实经同一 on_order_event 通道更新配对状
                // 态（Partial → Complete 或 Orphan）。判定链零改动：绑回是
                // 落账，不是输入。
                let key = self
                    .ome
                    .get(&order_id)
                    .map(|o| o.internal_key.clone())
                    .unwrap_or_else(|| format!("pair-chase:{naked_token}:{missing_token}:{slot}"));
                self.pair_book
                    .bind_by_leg(strategy, account_id.as_str(), missing_token, &key);
                true
            }
            Err(e) => {
                tracing::info!(
                    strategy = %strategy,
                    account = %account_id.as_str(),
                    token = %missing_token,
                    naked = %naked_token,
                    reason = %e.message,
                    "pair chase placement failed — falling back to alert+flatten"
                );
                false
            }
        }
    }

    /// #457: close a STILL-LIVE order the kernel judges settlement-dead —
    /// mark `ForceClosed`, release its reservation, and (live mode) queue a
    /// venue cancel so the real venue state converges with the book. Refuses
    /// an order that could still fill: the manual verb is for ghosts, and a
    /// caller that wants to withdraw a live order has `cancel`.
    pub fn force_close_order(&mut self, id: &str, now_ms: i64, reason: &str) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id.to_string()))?;
        if !order.status.is_live() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidOrderState,
                format!("order {id} is already terminal ({:?})", order.status),
            ));
        }
        if order.round_slot > 0 {
            // B1b: the round's liveness is judged on the ORDER'S OWN family
            // clock — the three-state eff (own stamp > market declaration >
            // global grid), not the configured grid: a 3600s order must stay
            // live for its full hour even while the core runs 900s rounds.
            // A `0` family folds to the global grid — the pre-B1b value —
            // and the resolution just made is the lazy idempotent stamp.
            let eff = self.order_eff_round_sec(&order);
            let dur_ms = eff * 1000;
            let expires_at = (order.round_slot + 1) * dur_ms;
            self.ome.backfill_round_duration(id, eff);
            if now_ms < expires_at {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidOrderState,
                    format!(
                        "order {id}'s round is still live until {expires_at} — use cancel, not force-close"
                    ),
                ));
            }
        }
        let was_live = order.status.is_live();
        if order.side == Side::Buy
            && let Ok(l) = self.accounts.get_mut(&order.account_id)
        {
            l.release(id);
        }
        // E-VA: expiry/force-close releases the virtual hold too.
        self.virtual_release_order(&order);
        self.ome
            .mark_terminal(id, OrderStatus::ForceClosed, now_ms)?;
        // RFC 568 阶段一：死亡事实（过期强平）喂配对簿。
        self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
        if let Some(engine) = self.engine.as_mut() {
            engine.release_pending_token_for_strategy(
                order.account_id.as_str(),
                &order.strategy,
                &order.token_id,
            );
        }
        if self.replay_sched() {
            self.fast_orders_dirty = true;
        }
        if was_live && let Some(vid) = order.venue_order_id {
            let venue = self.execution_venue_of(order.account_id.as_str(), &order.token_id);
            self.queue_venue_cancel(venue, vid);
        }
        tracing::warn!(
            order = %id,
            strategy = %order.strategy,
            token = %order.token_id,
            reason = %reason,
            "order force-closed by the kernel — round expired and no honest terminal receipt arrived (#457)"
        );
        // The audit trail's whole point: "we withdrew" (Cancelled) and "we gave
        // up waiting" (ForceClosed) must stay readable apart on disk.
        self.persist_audit_value(serde_json::json!({
            "kind": "force_close",
            "ts_ms": now_ms,
            "order_id": id,
            "strategy": order.strategy,
            "token_id": order.token_id,
            "round_slot": order.round_slot,
            "reason": reason,
        }));
        self.emit_order(id);
        Ok(())
    }
}
