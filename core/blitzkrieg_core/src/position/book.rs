use super::*;

impl PositionManager {
    /// Close a position, computing gross/net PnL and setting cooldowns.
    ///
    /// Everything here is derived from the ACCRUED cash flows (E17-d), so
    /// `net_pnl_usd` is exactly the cash the ledger moved for this position:
    ///
    /// ```text
    ///   gross = proceeds        − entry_cost
    ///   net   = gross − entry_fee − exit_fee
    /// ```
    ///
    /// An unsold remainder that cannot be placed on the venue's 0.01 share grid
    /// is written off at the exit price and charged to `net` as a rounding cost.
    /// That keeps the invariant true in the corner case instead of leaking a
    /// little cash every cycle, and it is visible on the record as `dust_shares`
    /// rather than hidden in a fudge.
    pub fn close(
        &mut self,
        position_id: &str,
        exit_price: Decimal,
        reason: ExitReason,
        was_maker: bool,
        now_ms: i64,
    ) -> Option<ClosedPosition> {
        let idx = self.open.iter().position(|p| p.id == position_id)?;
        let mut pos = self.open.remove(idx);
        pos.flows = pos.flows();

        // The caller's flag is the role of the ORDER it just placed; fold it in so
        // the record reflects every exit fill, including earlier partials.
        pos.exit_role = pos.exit_role.after_fill(if was_maker {
            OrderRole::Maker
        } else {
            OrderRole::Taker
        });

        let opened = pos.flows.opened_shares;
        let sold = pos.flows.sold_shares.min(opened);
        let dust = (opened - sold).max(Decimal::ZERO);
        let dust_notional = exit_price * dust;
        let exit_notional = pos.flows.proceeds_usd + dust_notional;

        // Shares with no exit fill of their own (a direct close, or the sub-grid
        // remainder) are priced here, so they must carry a fee here too — at the
        // role of the order the caller just placed. The fee follows the schedule
        // in force, through the one expression that prices a dust remainder.
        let dust_fee = dust_fee_for(
            crate::exit_policy::fee_schedule(),
            exit_price,
            dust_notional,
            was_maker,
        );

        let cost = pos.flows.entry_cost_usd;
        let entry_fee_usd = pos.flows.entry_fee_usd;
        let exit_fee_usd = pos.flows.exit_fee_usd + dust_fee;
        // The maker rebate is CASH the venue paid for this position's maker
        // fills (modeled in replay; zero elsewhere). It enters `net` with a
        // plus sign so `balance == seed + Σ net` keeps holding: the ledger
        // credited the same amount when the fills applied.
        let maker_rebate_usd = pos.flows.maker_rebate_usd;
        let gross = exit_notional - cost;
        let net = gross - entry_fee_usd - exit_fee_usd + maker_rebate_usd;
        let net_pct = if cost > Decimal::ZERO {
            (net / cost) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        let avg_exit = if sold > Decimal::ZERO {
            pos.flows.proceeds_usd / sold
        } else {
            exit_price
        };
        let exit_fee_pct = pct_of(exit_fee_usd, exit_notional);
        let hold_time_sec = (now_ms - pos.entered_at_ms) / 1000;

        // The day's realized loss lands in the CLOSED position's account bucket
        // (per-account budgets: one account's loss never burns a sibling's day).
        self.daily_by_account
            .entry(pos.account_id.clone())
            .or_default()
            .realized_pnl_usd += net;
        if reason == ExitReason::StopLoss {
            self.last_stop_loss_at = now_ms;
        }
        self.exit_cooldowns
            .insert(cooldown_key(&pos.asset, pos.direction), now_ms);
        self.asset_last_exit_at.insert(pos.asset.clone(), now_ms);
        if net < Decimal::ZERO {
            self.asset_last_loss_at.insert(pos.asset.clone(), now_ms);
        }
        // The freeze and its durable record travel WITH the loss (P0 #173).
        self.note_daily_drawdown(pos.account_id.as_str(), now_ms);
        self.last_suppression_at.remove(&pos.id);
        self.unpriceable_since.remove(&pos.id);
        // #390: the ladder ledger is keyed by position id; a closed id is
        // never revisited, so drop its run stamp with the position.
        self.exit_ladder_last_run.remove(&pos.id);
        // #464: the overlay ledgers die with the position for the same reason.
        self.ladder_steps_done.remove(&pos.id);
        self.ladder_stop_floor.remove(&pos.id);

        let closed = ClosedPosition {
            id: pos.id,
            strategy: pos.strategy,
            asset: pos.asset,
            direction: pos.direction,
            token_id: pos.token_id,
            condition_id: pos.condition_id,
            entry_price: pos.entry_price,
            exit_price: avg_exit,
            shares: opened,
            cost_usd: cost,
            was_maker_entry: entry_fee_usd == Decimal::ZERO,
            was_maker_exit: exit_fee_usd == Decimal::ZERO,
            entry_fee_pct: pct_of(entry_fee_usd, cost),
            exit_fee_pct,
            maker_rebate_usd,
            pnl_usd: gross,
            pnl_pct: pnl_pct(avg_exit, pos.entry_price),
            net_pnl_usd: net,
            net_pnl_pct: net_pct,
            high_pnl_pct: pos.state.high_pnl_pct,
            low_pnl_pct: pos.state.low_pnl_pct,
            hold_time_sec,
            exit_reason: reason,
            entered_at_ms: pos.entered_at_ms,
            exited_at_ms: now_ms,
            entry_role: pos.entry_role,
            exit_role: pos.exit_role,
            dust_shares: dust,
            // E28 (§9.2): the settled trade carries the position's account, so
            // per-account realized PnL is a field read.
            account_id: pos.account_id,
            // #427: the settled trade carries the position's venue.
            venue: pos.venue,
        };
        self.closed.push(closed.clone());
        if self.closed.len() > 5000 {
            let excess = self.closed.len() - 5000;
            self.closed.drain(0..excess);
        }
        Some(closed)
    }

    /// MERGE (task 2.2): burn complete UP+DOWN share-pairs of one condition into
    /// their on-chain collateral — $1.00 per pair, the collection channel of
    /// pair-discount arbitrage (@almach: 12,570 merges, zero sells).
    ///
    /// Each leg's paired shares leave at **$0.50/share** through the same
    /// `apply_exit_fill`/`close` bookkeeping every exit uses, so the two legs'
    /// exit notionals sum to exactly what [`crate::ledger::Ledger::credit_merge`]
    /// credits (`pairs × $1.00`) — the trade records and the cash ledger can
    /// never disagree. The merge itself pays NO fee: it is not a market order.
    ///
    /// Only complete pairs merge: `min(up_shares, down_shares)` floored to the
    /// share grid. A longer leg's remainder STAYS OPEN for settlement —
    /// unpaired shares have no collateral claim until the market resolves.
    /// Returns `None` (and changes nothing) unless both legs exist on the same
    /// account with at least one grid-legal pair: an on-chain merge is a
    /// single-wallet operation, and one-sided inventory has nothing to burn.
    pub fn merge_condition(&mut self, condition_id: &str, now_ms: i64) -> Option<MergeOutcome> {
        // Both legs must exist, on ONE account (a merge spends one wallet's
        // tokens on-chain; a cross-account pair is not mergeable).
        let up = self
            .open
            .iter()
            .find(|p| p.condition_id == condition_id && p.direction == SignalDirection::Up)
            .cloned()?;
        let down = self
            .open
            .iter()
            .find(|p| p.condition_id == condition_id && p.direction == SignalDirection::Down)
            .cloned()?;
        if up.account_id != down.account_id {
            tracing::warn!(
                condition = %condition_id,
                up_account = %up.account_id,
                down_account = %down.account_id,
                "merge refused: legs sit in different accounts — no single wallet owns the pair"
            );
            return None;
        }
        let pairs = floor_to_grid(up.shares).min(floor_to_grid(down.shares));
        if pairs <= Decimal::ZERO {
            return None;
        }

        // Burn the paired shares at $0.50/share per leg (fee-free, maker-role
        // bookkeeping: a merge is collateral collection, not a market exit).
        let merge_leg_price = Decimal::new(5, 1); // $0.50 per share
        let account_id = up.account_id.clone();
        // #647: the legs' own venue stamp — both legs are one account's tokens
        // on one condition, so one venue; empty when opened untagged. Read
        // before the burn moves the legs.
        let venue = up.venue.clone();
        let mut closed = Vec::new();
        for leg in [up, down] {
            // A merge is collateral collection, not a market exit: no fee and
            // no modeled rebate.
            let remaining = self.apply_exit_fill(
                &leg.id,
                pairs,
                merge_leg_price,
                Decimal::ZERO,
                OrderRole::Maker,
                Decimal::ZERO,
            );
            if remaining == Some(Decimal::ZERO) {
                // The merge consumed the whole leg: record the close. The fee
                // is zero and the exit notionals already sit in `flows`.
                if let Some(c) =
                    self.close(&leg.id, merge_leg_price, ExitReason::Merge, true, now_ms)
                {
                    closed.push(c);
                }
            }
        }
        // A merge is collateral collection, NOT a market exit: close() armed the
        // exit/asset cooldowns unconditionally, but the merged inventory is GONE
        // — @almach's rhythm is merge-then-reenter round over round, and the
        // next round's entry is a fresh decision on a fresh book, not a
        // rapid-fire re-entry into the same one. The $0.50/leg accounting makes
        // one leg's sign red by construction (entry 0.60 → exit 0.50), so a
        // per-leg "loss" is an artifact, not a signal: no loss cooldown either.
        // (A genuinely red PAIR — entry sum > 1 — is a strategy bug the entry
        // gate forbids; it never reaches this code.)
        for c in &closed {
            self.exit_cooldowns
                .remove(&cooldown_key(&c.asset, c.direction));
            self.asset_last_exit_at.remove(&c.asset);
            self.asset_last_loss_at.remove(&c.asset);
        }
        let unpaired = {
            let mut unpaired = Decimal::ZERO;
            for leg in self.open.iter() {
                if leg.condition_id == condition_id {
                    unpaired += leg.shares;
                }
            }
            unpaired
        };
        let outcome = MergeOutcome {
            condition_id: condition_id.to_string(),
            account_id,
            venue,
            pairs_merged: pairs,
            cash_credited_usd: pairs, // $1.00 per pair
            closed,
            unpaired_shares: unpaired,
        };
        Some(outcome)
    }

    /// Sell size for a full exit: exactly the shares held, floored to the venue's
    /// 0.01 share grid so the order can never oversell (E17-d).
    ///
    /// Before E17 this subtracted a flat 0.01 buffer, which left that buffer
    /// unsold on an exact-grid position — permanent dust, and cash that could
    /// never be reconciled. Flooring to the grid gives the same oversell
    /// protection with no remainder: a position of 10 sells 10, not 9.99.
    pub fn sell_shares(&self, position_id: &str) -> Option<Decimal> {
        let pos = self.open.iter().find(|p| p.id == position_id)?;
        let grid = floor_to_grid(pos.shares);
        if grid <= Decimal::ZERO {
            return None;
        }
        Some(grid)
    }

    /// F4: undo one `close()`. The venue reported the closing fill FAILED, so
    /// the position was never sold: put the pre-close row back (shares, basis,
    /// accrued flows, exit state — exactly as it stood), hand back the daily
    /// PnL the close added, and drop the closed record from the in-memory book
    /// so `balance == seed + Σ closed.net_pnl` holds again.
    ///
    /// The close-time cooldowns (`exit_cooldowns`, `asset_last_*`) are left
    /// alone on purpose: they only delay actions conservatively, and the
    /// pre-close values they overwrote are gone — trying to fake them would
    /// just move the drift.
    ///
    /// Returns false (and changes nothing) if the position id is already open,
    /// so a repeated unwind cannot duplicate the row.
    pub fn restore_closed(&mut self, pre_close: OpenPosition, closed: &ClosedPosition) -> bool {
        if self.open.iter().any(|p| p.id == pre_close.id) {
            return false;
        }
        // The deploy line keeps the day's realized PnL inside the account's
        // `DailyLossState` (#173), so the reversal is the exact inverse of
        // `close()`'s credit — into the SAME account's bucket.
        self.daily_by_account
            .entry(closed.account_id.clone())
            .or_default()
            .realized_pnl_usd -= closed.net_pnl_usd;
        if let Some(i) = self
            .closed
            .iter()
            .rposition(|c| c.id == closed.id && c.exited_at_ms == closed.exited_at_ms)
        {
            self.closed.remove(i);
        }
        self.open.push(pre_close);
        true
    }

    /// Capacity + cooldown gate (mirrors TS `canOpen`).
    ///
    /// The two capacity caps count only UNEXPIRED positions: a position
    /// whose round has passed `expires_at_ms` is settlement-dead (the
    /// winner redeems, the loser pays zero — no future event can grow the
    /// exposure), yet it can be stuck on the book indefinitely when no bid
    /// ever returns (the 2026-10-11 production incident: two expired
    /// positions held both shipped slots for 6 days; a restart restores the
    /// book, so the freeze never cleared on its own). Cooldowns and the
    /// daily-loss breaker keep judging every position as before.
    ///
    /// `account` scopes BOTH capacity caps: `max_positions` and
    /// `max_positions_per_asset` are per-account books, not one shared pot —
    /// account A filling its slots must not freeze account B's fresh entry.
    /// Cooldowns stay GLOBAL: they exist to stop the strategy from re-firing
    /// into the same wound, and two accounts hold the same asset by choice.
    /// The `default` scope passes the pre-scope positions unchanged (a
    /// single-book deployment counts exactly what it counted before).
    pub fn can_open(
        &self,
        account: &str,
        asset: Option<&str>,
        direction: Option<SignalDirection>,
        now_ms: i64,
    ) -> Result<(), String> {
        let live = self
            .open
            .iter()
            .filter(|p| p.expires_at_ms > now_ms && p.account_id.as_str() == account)
            .count();
        if live >= self.config.max_positions {
            return Err(format!("Max positions ({})", self.config.max_positions));
        }
        if self.daily_loss_tripped_for(account) {
            let daily = self.daily_bucket(account);
            let limit = if daily.tripped_limit_usd > Decimal::ZERO {
                daily.tripped_limit_usd
            } else {
                self.effective_daily_loss_limit_for(account)
            };
            return Err(format!(
                "Daily loss limit for account {account} (${limit}; realized ${}, day {})",
                daily.realized_pnl_usd,
                daily.day_index.unwrap_or(0)
            ));
        }
        if self.config.stop_loss_cooldown_sec > 0
            && self.last_stop_loss_at > 0
            && now_ms - self.last_stop_loss_at < self.config.stop_loss_cooldown_sec * 1000
        {
            let left = (self.config.stop_loss_cooldown_sec * 1000
                - (now_ms - self.last_stop_loss_at))
                / 1000
                + 1;
            return Err(format!("SL cooldown: {left}s"));
        }
        if let Some(asset) = asset {
            // #363: the per-asset cap is a COUNT now (`max_positions_per_asset`,
            // default 1 = the historical one-per-asset behaviour), not a binary
            // "already in?" — the @Almach replay runs concurrent same-asset
            // entries across rounds, and the execution policy / CLI raises this
            // cap for strategies that model that. The refusal KEEPS the exact
            // "Already in {asset}" text: `classify_rejection` buckets it as
            // `positions.already_in` and the WebUI attribution check pins the
            // vocabulary, so a wording drift would be a silent telemetry break.
            // At the old cap of 1 the message reads exactly as before; above it,
            // the count rides the same line so an operator can see why.
            let in_asset = self
                .open
                .iter()
                .filter(|p| {
                    p.asset == asset && p.expires_at_ms > now_ms && p.account_id.as_str() == account
                })
                .count();
            if in_asset >= self.config.max_positions_per_asset as usize {
                return Err(format!("Already in {asset}"));
            }
        }
        if let (Some(asset), Some(direction)) = (asset, direction) {
            let key = cooldown_key(asset, direction);
            if let Some(&last) = self.exit_cooldowns.get(&key)
                && now_ms - last < self.config.exit_cooldown_sec * 1000
            {
                let left = (self.config.exit_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                return Err(format!(
                    "Exit cooldown {asset} {}: {left}s",
                    direction.as_str()
                ));
            }
        }
        if let Some(asset) = asset {
            if self.config.loss_cooldown_sec > 0
                && let Some(&last) = self.asset_last_loss_at.get(asset)
                && now_ms - last < self.config.loss_cooldown_sec * 1000
            {
                let left = (self.config.loss_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                return Err(format!("Loss cooldown {asset}: {left}s"));
            }
            if self.config.asset_cooldown_sec > 0
                && let Some(&last) = self.asset_last_exit_at.get(asset)
                && now_ms - last < self.config.asset_cooldown_sec * 1000
            {
                let left = (self.config.asset_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                return Err(format!("Asset cooldown {asset}: {left}s"));
            }
        }
        Ok(())
    }
}
