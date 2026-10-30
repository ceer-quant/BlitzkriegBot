use super::*;

impl PositionManager {
    pub fn open(&mut self, p: OpenParams, now_ms: i64) -> OpenPosition {
        let id = format!("hft-{}", self.next_id);
        self.next_id += 1;
        // The requested role only seeds the view; the fee and the role are fixed
        // by the actual entry fill(s) in `apply_entry_fill` (E17-b).
        let entry_role = if p.was_maker {
            OrderRole::Maker
        } else {
            OrderRole::Taker
        };
        let pos = OpenPosition {
            id,
            strategy: p.strategy,
            asset: p.asset,
            direction: p.direction,
            token_id: p.token_id,
            condition_id: p.condition_id,
            entry_price: p.entry_price,
            current_price: p.entry_price,
            prev_price: p.entry_price,
            shares: Decimal::ZERO,
            cost_usd: Decimal::ZERO,
            was_maker_entry: p.was_maker,
            entry_fee_pct: Decimal::ZERO,
            entry_role,
            exit_role: OrderRole::Pending,
            target_exit_price: p.target_exit_price,
            entered_at_ms: now_ms,
            expires_at_ms: p.expires_at_ms,
            account_id: p.account_id,
            venue: p.venue,
            partner_token_id: p.partner_token_id,
            last_book_ts: 0,
            // B1b: stamped from the opening path's family resolution — `0`
            // only when the caller had no family fact (older callers).
            round_duration_sec: p.round_duration_sec,
            // The entry order's grid slot, stamped by the opening path. `0`
            // = the caller had no slot fact; expiry then falls back to the
            // `expires_at_ms` countdown (legacy behavior).
            round_slot: p.round_slot,
            state: ExitState::new(p.entry_price, now_ms),
            flows: CashFlows::default(),
        };
        self.open.push(pos.clone());
        pos
    }

    /// Accrue one ENTRY fill: basis, fee, shares and the resolved role — all from
    /// the amounts the ledger actually moved. Returns the new position snapshot.
    ///
    /// `shares`/`fee_usd` may be NEGATIVE (a `FAILED` rollback of an earlier
    /// fill): the accrual then reverses by the same amounts, so the position ends
    /// where it started instead of keeping phantom shares the cash ledger has
    /// already refunded.
    pub fn apply_entry_fill(
        &mut self,
        position_id: &str,
        shares: Decimal,
        price: Decimal,
        fee_usd: Decimal,
        role: OrderRole,
    ) -> Option<OpenPosition> {
        let pos = self.open.iter_mut().find(|p| p.id == position_id)?;
        let signed_cost = price * shares;
        pos.flows.opened_shares = (pos.flows.opened_shares + shares).max(Decimal::ZERO);
        pos.flows.entry_cost_usd = (pos.flows.entry_cost_usd + signed_cost).max(Decimal::ZERO);
        pos.flows.entry_fee_usd = (pos.flows.entry_fee_usd + fee_usd).max(Decimal::ZERO);
        // Basis and share count move together, so `cost_usd / shares` is always
        // the average paid for the shares still held.
        pos.shares = (pos.shares + shares).max(Decimal::ZERO);
        pos.cost_usd = (pos.cost_usd + signed_cost).max(Decimal::ZERO);
        if pos.shares == Decimal::ZERO {
            pos.cost_usd = Decimal::ZERO;
        }
        if shares > Decimal::ZERO {
            // A reversal is not a fill, so it neither picks a role nor re-anchors
            // the exit state.
            pos.entry_role = pos.entry_role.after_fill(role);
        }
        Self::refresh_view(pos);
        // Averaging into an existing position moves the basis, so the exit state
        // must follow — otherwise HWM/trails stay anchored to a stale entry.
        if shares > Decimal::ZERO {
            pos.state = ExitState::new(pos.entry_price, pos.entered_at_ms);
        }
        Some(pos.clone())
    }

    /// Accrue one MAKER fill's modeled rebate onto the position's cash flows
    /// (the entry side; a maker EXIT fill's rebate is accrued by
    /// [`Self::apply_exit_fill`]'s caller through the same method). The caller
    /// credits the ledger in the same breath, so the rebate is money the trade
    /// record AND the cash ledger both show.
    ///
    /// A negative `shares` (a FAILED rollback) reverses by the same rule the
    /// other accruals use — the rebate the venue never paid must not stay on
    /// the books (floored at zero, like every flow, so a rollback past a
    /// settled boundary cannot mint a negative rebate).
    pub fn accrue_maker_rebate(&mut self, position_id: &str, rebate_usd: Decimal) {
        if rebate_usd == Decimal::ZERO {
            return;
        }
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            pos.flows.maker_rebate_usd =
                (pos.flows.maker_rebate_usd + rebate_usd).max(Decimal::ZERO);
        }
    }

    /// B1b: idempotent lazy backfill of the position's round-length family —
    /// the manager-flavoured wrapper over
    /// [`OpenPosition::backfill_round_duration`]. A non-0 stamp is never
    /// overwritten (the first family a record was admitted under stays
    /// authoritative), so calling this on every fill / sweep pass is free and
    /// stable. The whole state lives under the single-threaded Core lock
    /// (`ipc/server.rs` serializes every verb and sweep), so the write-back
    /// rides the ordinary field-assignment path — no new lock.
    pub fn backfill_round_duration(&mut self, position_id: &str, sec: i64) {
        if sec <= 0 {
            return;
        }
        if let Some(p) = self.open.iter_mut().find(|p| p.id == position_id) {
            p.backfill_round_duration(sec);
        }
    }

    /// Accrue one EXIT fill (a partial close). Returns the position's new share
    /// count, or None if the position is unknown. A negative `shares` reverses an
    /// earlier exit fill (rolled back by the venue) and returns its basis.
    ///
    /// `maker_rebate_usd` is the modeled rebate this exit fill's MAKER role
    /// earned (zero for a taker exit / when the replay models no rebate). It
    /// folds into the same flows the close reads, so the trade record's net
    /// carries it exactly once.
    pub fn apply_exit_fill(
        &mut self,
        position_id: &str,
        shares: Decimal,
        price: Decimal,
        fee_usd: Decimal,
        role: OrderRole,
        maker_rebate_usd: Decimal,
    ) -> Option<Decimal> {
        let pos = self.open.iter_mut().find(|p| p.id == position_id)?;
        pos.flows.sold_shares = (pos.flows.sold_shares + shares).max(Decimal::ZERO);
        pos.flows.proceeds_usd = (pos.flows.proceeds_usd + price * shares).max(Decimal::ZERO);
        pos.flows.exit_fee_usd = (pos.flows.exit_fee_usd + fee_usd).max(Decimal::ZERO);
        pos.flows.maker_rebate_usd =
            (pos.flows.maker_rebate_usd + maker_rebate_usd).max(Decimal::ZERO);
        if shares > Decimal::ZERO {
            pos.exit_role = pos.exit_role.after_fill(role);
        }
        // Basis leaves at the position's own average, so what remains stays
        // proportional to the shares still held (no drift on a partial ladder).
        // A reversal adds it back at that same average.
        let per_share = if pos.shares > Decimal::ZERO {
            pos.cost_usd / pos.shares
        } else {
            Decimal::ZERO
        };
        let released = (per_share * shares).min(pos.cost_usd);
        pos.shares = (pos.shares - shares).max(Decimal::ZERO);
        pos.cost_usd = (pos.cost_usd - released).max(Decimal::ZERO);
        if pos.shares == Decimal::ZERO {
            pos.cost_usd = Decimal::ZERO;
        }
        Some(pos.shares)
    }

    /// Drop an open position that never really existed (a fully rolled-back
    /// entry leaves zero shares and zero basis; it must not block `can_open` or
    /// show up as an empty row on the panel). Returns true when one was dropped.
    pub fn drop_if_empty(&mut self, position_id: &str) -> bool {
        let idx = self.open.iter().position(|p| {
            p.id == position_id && p.shares <= Decimal::ZERO && p.cost_usd == Decimal::ZERO
        });
        match idx {
            Some(i) => {
                self.open.remove(i);
                true
            }
            None => false,
        }
    }
}
