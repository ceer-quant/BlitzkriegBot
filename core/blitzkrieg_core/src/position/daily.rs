use super::*;

impl PositionManager {
    /// The account's day-realized PnL (its OWN bucket — a sibling account's
    /// day never moves this number).
    pub fn daily_pnl_for(&self, account: &str) -> Decimal {
        self.daily_bucket(account).realized_pnl_usd
    }

    /// The process-wide day realized — every account's bucket summed. This is
    /// the SYSTEMIC fact (the `global_kill_switch_loss_usd` cap reads it); the
    /// per-account caps read [`Self::daily_pnl_for`] instead.
    pub fn daily_pnl_total(&self) -> Decimal {
        self.daily_by_account
            .values()
            .map(|s| s.realized_pnl_usd)
            .sum()
    }

    pub(super) fn daily_bucket(&self, account: &str) -> &DailyLossState {
        static EMPTY: std::sync::OnceLock<DailyLossState> = std::sync::OnceLock::new();
        self.daily_by_account
            .get(&AccountId::from(account))
            .unwrap_or_else(|| EMPTY.get_or_init(DailyLossState::default))
    }

    /// The day's realized-loss budget as the panel/CLI report it — ONE
    /// account's day (the caller names whose; the snapshot serves the display
    /// account's and carries the whole map beside it).
    pub fn daily_state_for(&self, account: &str) -> DailyLossState {
        self.daily_bucket(account).clone()
    }

    /// Every account's day, for views that must show all books at once.
    pub fn daily_states(&self) -> &std::collections::BTreeMap<AccountId, DailyLossState> {
        &self.daily_by_account
    }

    /// #363: the account's net realized PnL over the TRAILING HOUR, read
    /// straight off the closed-trade list (`exited_at_ms` is the close's own
    /// timestamp, so a restart loses the window until the first fresh close —
    /// the same session-scope the streak counter has; a policy that needs the
    /// last hour across restarts should gate on `consecutive_losses` instead,
    /// which is persisted). Per-account by field, the E28 way — never a
    /// strategy-name heuristic.
    pub fn recent_pnl_1h_for(&self, account_id: &str, now_ms: i64) -> Decimal {
        let window = 3_600_000i64;
        self.closed
            .iter()
            .filter(|c| c.account_id.as_str() == account_id && now_ms - c.exited_at_ms <= window)
            .map(|c| c.net_pnl_usd)
            .sum()
    }

    /// The open positions' unrealized PnL (USD, negative = paper loss), each
    /// position marked `(current_price − entry_price) × shares` off its last
    /// known reference price. Positions with no shares, no mark (`current_price
    /// ≤ 0`) or an absurd share count contribute 0 — never a fabricated profit
    /// or loss.
    ///
    /// `ponytail:` the mark is the LAST KNOWN price: `current_price` is
    /// refreshed from live books per tick, but a book that has gone quiet
    /// leaves the last value standing and there is no staleness gate on it
    /// (Issue #464 considered one; upgrade path = a `last_book_ts`-age check
    /// here, which `OpenPosition` already carries).
    pub fn unrealized_pnl_usd(&self) -> Decimal {
        self.open
            .iter()
            .filter(|p| p.shares > Decimal::ZERO && p.current_price > Decimal::ZERO)
            .map(|p| (p.current_price - p.entry_price) * p.shares)
            .sum()
    }

    /// ONE account's open book, same mark discipline as
    /// [`Self::unrealized_pnl_usd`] — the per-account daily breaker judges its
    /// own paper loss, never a sibling's.
    pub fn unrealized_pnl_usd_for(&self, account: &str) -> Decimal {
        self.open
            .iter()
            .filter(|p| p.account_id.as_str() == account)
            .filter(|p| p.shares > Decimal::ZERO && p.current_price > Decimal::ZERO)
            .map(|p| (p.current_price - p.entry_price) * p.shares)
            .sum()
    }

    /// The number the daily-loss breaker judges on (#464): the day's realized
    /// loss plus — when [`PositionConfig::daily_loss_marks_unrealized`] is on —
    /// the open book's unrealized loss. The realized-only reading ships OFF;
    /// setting that flag `false` reproduces the pre-#464 input exactly. The
    /// persisted [`DailyLossState::realized_pnl_usd`] is NOT this number: the
    /// unrealized half is a JUDGEMENT input, never an accounting entry.
    /// Per-account: the account's own realized, the account's own paper.
    fn daily_drawdown_input(&self, account: &str) -> Decimal {
        let realized = self.daily_bucket(account).realized_pnl_usd;
        if self.config.daily_loss_marks_unrealized {
            realized + self.unrealized_pnl_usd_for(account)
        } else {
            realized
        }
    }

    /// The cap for ONE account's day (its own opening equity base).
    pub fn effective_daily_loss_limit_for_public(&self, account: &str) -> Decimal {
        self.effective_daily_loss_limit_for(account)
    }

    /// The cap for ONE account's day (its own opening equity base).
    pub(super) fn effective_daily_loss_limit_for(&self, account: &str) -> Decimal {
        let abs = self.config.max_daily_loss_usd;
        let pct = self.config.max_daily_loss_equity_pct;
        let opening = self.daily_bucket(account).opening_equity_usd;
        let rel = if pct > Decimal::ZERO && opening > Decimal::ZERO {
            opening * pct / Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        match (abs > Decimal::ZERO, rel > Decimal::ZERO) {
            (true, true) => abs.min(rel),
            (true, false) => abs,
            (false, true) => rel,
            (false, false) => Decimal::ZERO,
        }
    }

    /// True when the day's net loss (see [`Self::daily_drawdown_input`])
    /// reached the effective cap. Sticky for the rest of the day so a later
    /// winning trade cannot re-open entries.
    pub fn daily_loss_tripped_for(&self, account: &str) -> bool {
        let daily = self.daily_bucket(account);
        if daily.tripped {
            return true;
        }
        let limit = self.effective_daily_loss_limit_for(account);
        limit > Decimal::ZERO && self.daily_drawdown_input(account) <= -limit
    }

    /// Roll the budget over when `now_ms` crosses into a new UTC day, seeding
    /// the new day's opening equity from `equity_usd` (the caller's cash
    /// equity: the position manager cannot read the ledger itself).
    ///
    /// `basis` says WHICH book that equity came from (P1 #235) — the caller
    /// reads it off the mode, because the same ledger balance means a different
    /// thing in dry and in live. Within a day it is what tells a re-anchored
    /// budget apart from the same account still trading, so it is a parameter
    /// rather than a default: a caller that forgets it would silently keep the
    /// fail-open behaviour this exists to close.
    ///
    /// Returns `Some` exactly when a new day opened, so the caller can put the
    /// old day's result in the audit trail. The FIRST call only stamps the day
    /// (nothing to close yet) and reports it, so the budget's start is visible.
    /// A same-day re-anchor is NOT a new day and reports `None`; it is announced
    /// by its own warning, and by the trip alert when it freezes the day.
    pub fn roll_daily(
        &mut self,
        account: &str,
        now_ms: i64,
        equity_usd: Decimal,
        basis: EquityBasis,
    ) -> Option<DailyRoll> {
        let today = utc_day_index(now_ms);
        if self
            .daily_by_account
            .get(&AccountId::from(account))
            .map(|d| d.day_index)
            .unwrap_or(None)
            == Some(today)
        {
            self.reconcile_same_day_base(account, now_ms, equity_usd, basis);
            return None;
        }
        let daily = self
            .daily_by_account
            .entry(AccountId::from(account))
            .or_default();
        let previous_day_index = daily.day_index;
        let previous_realized = daily.realized_pnl_usd;
        let previous_tripped = daily.tripped;
        let carried_pnl = if previous_day_index.is_none() {
            // First stamp of the day (fresh process, or before this field
            // existed): it only starts the clock. Whatever was realized before
            // the first tick belongs to TODAY, so it is carried, never zeroed.
            previous_realized
        } else {
            // A real boundary: the closed day's result is reported, then the new
            // day starts from zero.
            Decimal::ZERO
        };
        let carried_trip = previous_day_index.is_none() && previous_tripped;
        let carried_at = daily.tripped_at_ms;
        let carried_limit = daily.tripped_limit_usd;
        *daily = DailyLossState {
            day_index: Some(today),
            realized_pnl_usd: carried_pnl,
            opening_equity_usd: equity_usd.max(Decimal::ZERO),
            equity_basis: Some(basis),
            tripped: carried_trip,
            tripped_at_ms: if carried_trip { carried_at } else { 0 },
            tripped_limit_usd: if carried_trip {
                carried_limit
            } else {
                Decimal::ZERO
            },
            tripped_reported: false,
            opened_at_ms: now_ms,
        };
        // A REAL UTC-day boundary for this account refreshes the strategy
        // cooldowns; a brand-new account's first stamp (previous_day_index
        // None) must NOT — it would wipe siblings' same-day exit cooldowns
        // mid-day. (The single-slot state cleared these unconditionally: one
        // book, one day.)
        if previous_day_index.is_some() {
            self.last_stop_loss_at = 0;
            self.exit_cooldowns.clear();
        }
        self.persist_daily();
        Some(DailyRoll {
            day_index: today,
            previous_day_index,
            previous_realized_pnl_usd: previous_realized,
            opening_equity_usd: self.daily_bucket(account).opening_equity_usd,
            limit_usd: self.effective_daily_loss_limit_for(account),
            previous_tripped,
        })
    }

    /// Keep the day's opening equity honest WITHIN the day (P1 #235).
    ///
    /// The cap is a share of the account the day opened on, and only one thing
    /// may move that base mid-day: a base that was measured against a DIFFERENT
    /// book. Without this, a dry run that stamped the day with its $10 000 seed
    /// leaves a live $50 account carrying a $2 000 cap — and since
    /// [`Self::effective_daily_loss_limit`] takes the TIGHTER of the absolute
    /// and the relative cap, an inflated base inflates the cap: the breaker
    /// cannot trip. That is the fail-open direction, so it is closed here.
    ///
    /// Two conditions re-anchor, and both are clamped to the safe direction:
    ///
    /// * the basis changed (dry↔live — including a state file that never
    ///   recorded one, which cannot prove it belongs to the book in front of
    ///   it), or
    /// * the book moved by more than [`same_day_rebase_deviation_pct`], the
    ///   "same label, different account size" case the basis alone misses.
    ///
    /// A re-anchor may only TIGHTEN. A larger book keeps the day's smaller base,
    /// because re-anchoring upward would hand the day a budget it never had —
    /// the very hole this closes. That is also why a same-basis restart at
    /// (roughly) the same equity changes nothing and says nothing: a restart
    /// must not disturb how much of the day's budget is already spent.
    fn reconcile_same_day_base(
        &mut self,
        account: &str,
        now_ms: i64,
        equity_usd: Decimal,
        basis: EquityBasis,
    ) {
        if equity_usd <= Decimal::ZERO {
            // No account size to measure against (a live process whose balance
            // has not arrived yet): keep whatever base the day has.
            return;
        }
        let daily = self
            .daily_by_account
            .entry(AccountId::from(account))
            .or_default();
        let old_equity = daily.opening_equity_usd;
        let old_basis = daily.equity_basis;
        if old_equity <= Decimal::ZERO {
            // Stamped before the account size was known: adopt today's book.
            daily.opening_equity_usd = equity_usd;
            daily.equity_basis = Some(basis);
            self.persist_daily();
            return;
        }
        let basis_changed = old_basis != Some(basis);
        let moved_pct = ((equity_usd - old_equity).abs() / old_equity) * Decimal::ONE_HUNDRED;
        if !basis_changed && moved_pct <= same_day_rebase_deviation_pct() {
            return;
        }
        if equity_usd >= old_equity {
            // The other book is bigger: re-anchoring would LOOSEN the day's cap,
            // so the day keeps the tighter base it already has.
            return;
        }
        daily.opening_equity_usd = equity_usd;
        daily.equity_basis = Some(basis);
        let day_index = daily.day_index.unwrap_or(0);
        let realized = daily.realized_pnl_usd;
        let limit = self.effective_daily_loss_limit_for(account);
        let breach = self.daily_drawdown_input(account);
        let breached = limit > Decimal::ZERO && breach <= -limit;
        if breached {
            // The tightened cap is already spent. Latch the trip HERE rather
            // than letting the next tick rediscover it, so the day cannot read
            // as healthy in between, and re-arm the report: the number the
            // operator was shown has moved. Exits are untouched either way —
            // the trip freezes entries (see `can_open`).
            if let Some(daily) = self.daily_by_account.get_mut(&AccountId::from(account)) {
                if !daily.tripped {
                    daily.tripped = true;
                    daily.tripped_at_ms = now_ms;
                }
                daily.tripped_limit_usd = limit;
                daily.tripped_reported = false;
            }
        }
        let old_basis_str = old_basis.map(EquityBasis::as_str).unwrap_or("unrecorded");
        tracing::warn!(
            account = %account,
            day = day_index,
            old_equity = %old_equity,
            new_equity = %equity_usd,
            old_basis = old_basis_str,
            new_basis = basis.as_str(),
            realized = %realized,
            limit = %limit,
            tripped = breached,
            "daily loss budget re-anchored within the UTC day: opening equity {} ({}) -> {} ({}){}",
            old_equity,
            old_basis_str,
            equity_usd,
            basis.as_str(),
            if breached {
                " — the tightened cap is already spent, so entries are frozen for the rest of the day (exits are unaffected)"
            } else {
                ""
            }
        );
        self.persist_daily();
    }

    /// Every account's un-announced trip, latched and consumed. The caller
    /// announces one line per account — a sibling account's freeze must not
    /// hide behind another's.
    pub fn take_daily_trips(&mut self) -> Vec<(AccountId, DailyTrip)> {
        let mut trips = Vec::new();
        let accounts: Vec<AccountId> = self.daily_by_account.keys().cloned().collect();
        for account in accounts {
            if !self.daily_loss_tripped_for(account.as_str()) {
                continue;
            }
            // The latch cap is computed BEFORE the mutable borrow (it reads
            // the bucket's opening equity through the same map).
            let latch_limit = self.effective_daily_loss_limit_for(account.as_str());
            let (day_index, realized, opening, limit, at_ms) = {
                let daily = self
                    .daily_by_account
                    .get_mut(&account)
                    .expect("bucket exists (keys come from the map)");
                if !daily.tripped {
                    // Detected here rather than only in `close` (a restored budget can
                    // arrive already breached): latch it so the panel agrees.
                    daily.tripped = true;
                    daily.tripped_at_ms = daily.opened_at_ms;
                    daily.tripped_limit_usd = latch_limit;
                }
                if daily.tripped_reported {
                    continue;
                }
                daily.tripped_reported = true;
                (
                    daily.day_index.unwrap_or(0),
                    daily.realized_pnl_usd,
                    daily.opening_equity_usd,
                    daily.tripped_limit_usd,
                    daily.tripped_at_ms,
                )
            };
            self.persist_daily();
            trips.push((
                account,
                DailyTrip {
                    day_index,
                    realized_pnl_usd: realized,
                    limit_usd: limit,
                    opening_equity_usd: opening,
                    at_ms,
                },
            ));
        }
        trips
    }

    fn persist_daily(&self) {
        if let Some(store) = self.daily_store.as_ref() {
            store.save(&self.daily_by_account);
        }
    }

    /// Latch the breaker if the day's realized loss just reached the cap. Called
    /// from [`close`](Self::close) so the freeze and its persistence happen with
    /// the loss, not one tick later — against the CLOSED position's account.
    pub(super) fn note_daily_drawdown(&mut self, account: &str, now_ms: i64) {
        let limit = self.effective_daily_loss_limit_for(account);
        if limit > Decimal::ZERO
            && self.daily_drawdown_input(account) <= -limit
            && !self.daily_bucket(account).tripped
            && let Some(daily) = self.daily_by_account.get_mut(&AccountId::from(account))
        {
            daily.tripped = true;
            daily.tripped_at_ms = now_ms;
            daily.tripped_limit_usd = limit;
            tracing::warn!(
                account = %account,
                day = daily.day_index.unwrap_or(0),
                realized = %daily.realized_pnl_usd,
                limit = %limit,
                equity = %daily.opening_equity_usd,
                "daily loss limit reached — new entries frozen for this account until the next UTC day"
            );
        }
        self.persist_daily();
    }
}
