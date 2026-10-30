use super::*;

impl Core {
    /// Breaker + event emission when a position closes.
    pub(crate) fn on_position_closed(
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
    pub(crate) fn note_exit_reason(&mut self, token: &str, reason: ExitReason) {
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
    pub(crate) fn take_exit_reason(&mut self, token: &str) -> Option<ExitReason> {
        let reason = self.exit_reasons.remove(token);
        if reason.is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
        reason
    }

    /// Forget the pending reason for `token` without consuming it — the close
    /// that landed carried its own reason (a settlement, a reconciled venue
    /// close), so the in-flight one is stale either way.
    pub(crate) fn clear_exit_reason(&mut self, token: &str) {
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
    pub(crate) fn sweep_exit_reasons(&mut self) {
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
    pub(crate) fn enforce_exit_reason_bound(&mut self) {
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
    pub(crate) fn repair_exit_reason_order(&mut self) {
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
    pub(crate) fn exit_reasons_view(&self) -> serde_json::Value {
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
    pub(crate) fn orderbook_freshness_view(&self) -> serde_json::Value {
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
    pub(in crate::service) fn push_close_credential(
        &mut self,
        trade_id: String,
        cred: CloseCredential,
    ) {
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
    pub(in crate::service) fn unwind_closed_trade(
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
}
