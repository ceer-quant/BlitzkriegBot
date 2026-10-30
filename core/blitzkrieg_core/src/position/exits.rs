use super::*;

impl PositionManager {
    /// Fold the accrued flows back into the display/audit fields of an open
    /// position: the fee rate is a VIEW of the fee actually charged, and the
    /// entry price is the basis per share still held.
    pub(super) fn refresh_view(pos: &mut OpenPosition) {
        pos.entry_fee_pct = pct_of(pos.flows.entry_fee_usd, pos.flows.entry_cost_usd);
        pos.was_maker_entry = pos.flows.entry_fee_usd == Decimal::ZERO;
        if pos.shares > Decimal::ZERO {
            pos.entry_price = pos.cost_usd / pos.shares;
        }
    }

    /// Update a position's exit state from a fresh book.
    pub fn tick(&mut self, position_id: &str, book: Option<&OrderbookSnapshot>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            let view = book.map(BookView::from_snapshot);
            Self::valuate_one(pos, view.as_ref(), now_ms, &cfg);
        }
    }

    /// Update a single position's exit state AND its `current_price` from a book.
    /// This is what makes the UI's unrealized PnL move; it must run independently
    /// of whether automated exits are enabled.
    ///
    /// F6: `current_price` is a REFERENCE valuation (bid, else two-sided mid,
    /// else the stale value) — the dashboard may keep showing it. It is never
    /// again a substitute for an executable quote: exit decisions and SELL fills
    /// price off `executable_bid`, which is zero when no bid quotes.
    fn valuate_one(pos: &mut OpenPosition, book: Option<&BookView>, now_ms: i64, cfg: &ExitConfig) {
        update_exit_state(&mut pos.state, pos.entry_price, book, now_ms, cfg);
        let val = reference_price(book, pos.current_price);
        if val > Decimal::ZERO && pos.current_price != val {
            pos.prev_price = pos.current_price;
            pos.current_price = val;
        }
        // Track the freshness of the last book the valuation actually saw: the
        // SNAPSHOT's own receive time when the caller supplies one (backtest /
        // replay), else `now`. The exit path uses this to refuse pricing a SELL
        // off a stale quote. NOTE: the live service rebuilds cached snapshots
        // with `timestamp = now_ms`, which always looks fresh — preserving the
        // real receive time through that cache is a service-layer follow-up.
        if let Some(b) = book {
            pos.last_book_ts = if b.timestamp > 0 { b.timestamp } else { now_ms };
        }
    }

    /// Re-value every open position from the latest books (no exit decisions).
    /// Safe to call even when automated exits are disabled, so the dashboard
    /// always shows live unrealized PnL and HWM.
    pub fn valuate(&mut self, books: &dyn Fn(&str) -> Option<BookView>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        for pos in self.open.iter_mut() {
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
        }
    }

    /// Fast-path valuation (replay `backtest_fast`, all-holder books):
    /// re-value ONLY positions whose token is in `dirty_tokens` — a book
    /// actually arrived for it since the last pass. Every other position
    /// keeps its previous marks, provably what the full pass produced:
    /// `valuate_one` is a pure function of (book, now, cfg) and the book is
    /// unchanged between arrivals, so a quiet-tick re-run reproduces the
    /// same `current_price` (only the tick-count ratchet state stands
    /// still — unobservable, the fast path never consults the holder
    /// ladder). The Decimal-heavy pass over hundreds of idle positions
    /// stops running on every 50 ms maintenance tick.
    pub fn valuate_dirty_tokens(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        dirty_tokens: &std::collections::HashSet<String>,
    ) {
        if dirty_tokens.is_empty() {
            return;
        }
        let cfg = self.config.exit.clone();
        for pos in self.open.iter_mut() {
            if !dirty_tokens.contains(&pos.token_id) {
                continue;
            }
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
        }
    }

    /// Evaluate every open position; returns exit requests for those that hit a
    /// rule. Updates exit state from the book first so the decision and the
    /// recorded HWM share the same quote.
    pub fn check_exits(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
    ) -> Vec<ExitRequest> {
        self.check_exits_inner(books, now_ms, false, 0, &std::collections::HashSet::new())
    }

    /// Issue #390 (replay scheduling): refresh ONLY the display reference
    /// marks — `current_price` / `prev_price` / `last_book_ts` — without
    /// touching the exit state (the counter ratchet in particular). The base
    /// kernel's every-tick valuate advances these marks on every tick and its
    /// ladder runs right after, so a consumer priced off `current_price`
    /// between the two passes sees the fresh mark; the merged replay pass
    /// keeps that ordering visible by refreshing marks separately:
    ///
    ///   * on a SKIP tick, only dirty tokens — the base advanced every mark
    ///     from the same (unchanged) book a quiet re-run would read, so
    ///     refreshing just the tokens whose book changed reproduces it;
    ///   * on a RUN tick, every position — the ladder's own state update
    ///     runs after this and must observe the same marks the base's
    ///     valuate fed it (strategy-signal exits price off `current_price`).
    ///
    /// Deliberately NOT the ladder's `valuate_one`: that would advance the
    /// counter ratchet the interval backfill already owns.
    pub fn refresh_marks(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        all: bool,
        dirty_tokens: &std::collections::HashSet<String>,
    ) {
        for pos in self.open.iter_mut() {
            if !all && !dirty_tokens.contains(&pos.token_id) {
                continue;
            }
            let book = books(&pos.token_id);
            let val = reference_price(book.as_ref(), pos.current_price);
            if val > Decimal::ZERO && pos.current_price != val {
                pos.prev_price = pos.current_price;
                pos.current_price = val;
            }
            if let Some(b) = book {
                pos.last_book_ts = if b.timestamp > 0 { b.timestamp } else { now_ms };
            }
        }
    }

    /// Issue #390 (replay nofast): [`check_exits`] on a deadline schedule.
    ///
    /// `run_now` forces the ladder to run this tick (first pass, dirty token,
    /// deadline); otherwise it is skipped and the caller keeps the previous
    /// verdicts. `tick_boundary_ms` is the replay maintenance grid — the
    /// skipped-tick count the counter ratchet compensates over. Tokens in
    /// `dirty_tokens` (books that arrived since the last pass) must run.
    ///
    /// Output contract: the sequence of (books × ticks) the every-tick pass
    /// observes is partitioned by this pass into "runs" and "skips", and a
    /// skip is exactly a re-observation of the previous run's inputs at a
    /// later clock — its only durable effect is the counter ratchets, which
    /// the interval backfill reproduces (`update_exit_state_interval`). The
    /// timestamps every command and report carries stay `now_ms` — the tick
    /// the verdict actually fires on.
    pub fn check_exits_interval(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        run_now: bool,
        interval_mode: bool,
        tick_boundary_ms: i64,
        dirty_tokens: &std::collections::HashSet<String>,
    ) -> Vec<ExitRequest> {
        if !run_now {
            return Vec::new();
        }
        self.check_exits_inner(books, now_ms, interval_mode, tick_boundary_ms, dirty_tokens)
    }
    /// The shared ladder body. `interval_mode` selects the #390 spelling: the
    /// per-position state update is the merged maintenance update — ONE call
    /// per run at the DOUBLE (2×) backfill rate, because the base kernel's
    /// every-tick valuate pass and every-tick ladder each advance the same
    /// counter ratchet once per tick, and this single run replaces both (the
    /// per-position last-run ledger of the first #390 spelling is gone: two
    /// independent 1× backfills over one shared counter disagree with the
    /// base's joint walk on latch-mid-gap and reset-walk corners). The legacy
    /// spelling keeps `valuate_one` and never touches a ledger.
    ///
    /// The interval branch reuses the legacy tail VERBATIM through
    /// `finish_exit_tick` (the push/report section below it); only the state
    /// update differs (`update_exit_state_interval_2x`).
    fn check_exits_inner(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        interval_mode: bool,
        tick_boundary_ms: i64,
        _dirty_tokens: &std::collections::HashSet<String>,
    ) -> Vec<ExitRequest> {
        let cfg = self.config.exit.clone();
        let mut out = Vec::new();
        let mut withheld: Vec<SuppressedStopEvent> = Vec::new();
        for pos in self.open.iter_mut() {
            // The strategy's own trailing declaration rides ON TOP of the
            // kernel-wide config for THIS position's verdict; every other knob
            // stays shared. Cheap: an absent override short-circuits to the
            // borrowed config via an early match arm.
            // A hold-to-settlement declaration rides the SAME overlay point:
            // its positions' protective stop runs on the WIDE containment
            // width (`holder_effective_stop_pct`) instead of the 12% ladder —
            // the @almach compromise for the live ladder that keeps walking
            // until venue settlement is proven. The overlay swaps in the
            // holder VIEW of the config, so every stop reader downstream
            // (verdict functions, ladder step, suppressed-stop reports) sees
            // the same flat wide number the PHYSICS projection composes.
            let holder = self.holder_stop_overrides.contains_key(&pos.strategy);
            let trailing = self.trailing_overrides.get(&pos.strategy);
            let overridden;
            let pos_cfg = match (holder, trailing.copied()) {
                // No declaration at all: the borrowed config, byte-identical.
                (false, None) => &cfg,
                (false, Some(on)) if on == cfg.trailing_enabled => &cfg,
                // Trailing declaration only.
                (false, Some(on)) => {
                    overridden = ExitConfig {
                        trailing_enabled: on,
                        ..cfg.clone()
                    };
                    &overridden
                }
                // Holder (with or without a trailing declaration — the
                // kernel's trailing stays OFF by calibration; a `Some(on)`
                // still wins).
                (true, trailing_on) => {
                    overridden = ExitConfig {
                        trailing_enabled: trailing_on.unwrap_or(cfg.trailing_enabled),
                        ..cfg.holder_view()
                    };
                    &overridden
                }
            };
            let book = books(&pos.token_id);
            if interval_mode {
                crate::exit_policy::update_exit_state_interval_2x(
                    &mut pos.state,
                    pos.entry_price,
                    book.as_ref(),
                    now_ms,
                    pos_cfg,
                    self.exit_ladder_last_run
                        .get(&pos.id)
                        .copied()
                        .unwrap_or(now_ms),
                    tick_boundary_ms,
                );
                self.exit_ladder_last_run.insert(pos.id.clone(), now_ms);
            } else {
                Self::valuate_one(pos, book.as_ref(), now_ms, pos_cfg);
            }
            // F6: the exit price is an EXECUTABLE bid or nothing. The old code
            // fell back to `pos.current_price` — a stale reference that could
            // be an arbitrary number of seconds old, or a one-sided-book
            // phantom — and let forced exits book profit no buyer was offering.
            // A book older than the staleness budget is not a quote either: a
            // bid captured minutes ago is not a buyer standing here now.
            //
            // This gate prices the ORDER, and only the order. It must not skip
            // the DECISION: `decide_exit_verdict` already draws the same line
            // (a mandatory or profit-side exit needs a live bid; the protective
            // stop may read a fresh last-known price), and short-circuiting
            // here made a breached stop indistinguishable from a quiet market —
            // no order AND no report. So the verdict runs either way, and a
            // decision that cannot be priced becomes a held-stop report below.
            let exit_price = executable_bid(book.as_ref());
            // #268: the age is kept as well as the verdict on it, because
            // "stale" and "thin" need telling apart downstream and the age is
            // the only thing that can. `book_is_fresh` is the ONE spelling of
            // the budget question (an unknown age counts as fresh — see its
            // doc); a missing book has nothing to age and is left to the
            // judgement-side bounds.
            let book_age_ms = book.as_ref().and_then(|b| book_age_ms(b, now_ms));
            let book_fresh = book.as_ref().is_none_or(|b| book_is_fresh(b, now_ms, &cfg));
            let priceable = exit_price > Decimal::ZERO && book_fresh;
            // #268 item 2: how long this position has been unable to act. The
            // run starts at the first unpriceable tick and is cleared by the
            // first tick that can price it, so what the escalation below reads
            // is a continuous outage and never a lifetime total.
            let unpriceable_for_ms = if priceable {
                self.unpriceable_since.remove(&pos.id);
                0
            } else {
                let since = *self
                    .unpriceable_since
                    .entry(pos.id.clone())
                    .or_insert(now_ms);
                now_ms - since
            };
            let escalated =
                unpriceable_for_ms >= UNPRICEABLE_ESCALATION_BUDGETS * cfg.max_book_age_sec * 1_000;
            // A held exit names its reason: a stale book and a thin one are the
            // same zero to `executable_bid`, and only the age separates them.
            let hold_cause = if book_fresh {
                StopSuppressionCause::NoExecutableQuote
            } else {
                StopSuppressionCause::StaleBook
            };
            let time_left_sec = (pos.expires_at_ms - now_ms) / 1000;
            let hold_sec = (now_ms - pos.entered_at_ms) / 1000;

            // Fixed-target strategies (e.g. sharp_reversal).
            if let Some(t) = pos.target_exit_price
                && priceable
                && exit_price >= t
                && hold_sec >= self.config.exit.exit_grace_sec
            {
                out.push(ExitRequest {
                    position_id: pos.id.clone(),
                    reason: ExitReason::TakeProfit,
                    exit_price: t,
                    use_maker: true,
                    close_shares: None,
                });
                continue;
            }

            // The verdict carries back a protective stop the wick guard
            // withheld, so "should have triggered" is reportable rather than
            // silent (P0 #177).
            let ExitVerdict {
                decision,
                suppressed_stop,
                stop_reference,
            } = decide_exit_verdict(ExitTickInput {
                entry_price: pos.entry_price,
                book: book
                    .as_ref()
                    .map(|b| b as &dyn crate::exit_policy::BookScalarView),
                fallback_price: Some(pos.current_price),
                time_left_sec,
                hold_sec,
                state: &pos.state,
                now_ms,
                cfg: pos_cfg,
            });
            if let Some(s) = suppressed_stop {
                withheld.push(SuppressedStopEvent {
                    cause: StopSuppressionCause::WickGuard,
                    reason: Some(ExitReason::StopLoss),
                    position_id: pos.id.clone(),
                    token_id: pos.token_id.clone(),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    entry_price: pos.entry_price,
                    bid: s.bid,
                    mid: s.mid,
                    pnl_pct_at_bid: s.pnl_pct_at_bid,
                    pnl_pct_at_mid: s.pnl_pct_at_mid,
                    stop_pct: s.stop_pct,
                    book_age_ms,
                    unpriceable_for_ms,
                    escalated,
                    now_ms,
                });
            }
            // The held-exit report: an exit was warranted and no order could be
            // sent for it. Two things file one — a rule that fired with no buyer
            // to sell to at any price we can name (#225, #267), and a position
            // that has been unpriceable past the budget (#268, item 2). They
            // differ in ONE field, whether a rule named the reason, so they are
            // built once here: a field added for one path cannot go missing from
            // the other.
            let held_exit = |reason: Option<ExitReason>| {
                // What the exit was JUDGED on is the stop's own reference
                // whenever there is one (#225): a bid-less book whose ask
                // collapsed can only be judged on that ask, and `current_price`
                // never moved off the entry there — reporting it would have said
                // "flat" about the one case this report exists for. With no stop
                // reference (#267) the profit side's own judgement is what fired,
                // and `current_price` is the reference it read.
                let judged = stop_reference.map_or(pos.current_price, |r| r.price);
                SuppressedStopEvent {
                    cause: hold_cause,
                    reason,
                    position_id: pos.id.clone(),
                    token_id: pos.token_id.clone(),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    entry_price: pos.entry_price,
                    bid: exit_price,
                    mid: judged,
                    // A zero bid is no pnl to report: `pnl_pct` would call it
                    // -100%, which reads as a wipeout rather than as a market
                    // with no buyer in it.
                    pnl_pct_at_bid: if exit_price > Decimal::ZERO {
                        pnl_pct(exit_price, pos.entry_price)
                    } else {
                        Decimal::ZERO
                    },
                    pnl_pct_at_mid: pnl_pct(judged, pos.entry_price),
                    stop_pct: effective_stop_pct(pos_cfg.stop_loss_pct, time_left_sec, pos_cfg),
                    book_age_ms,
                    unpriceable_for_ms,
                    escalated,
                    now_ms,
                }
            };
            // Issue #464: the operator's explicit ladder runs on EVERY priceable
            // tick — not only where the policy's own TP/SL verdict fired —
            // because its stop floor (raised by `move_stop_to`) must patrol
            // quiet ticks too: a +2.5% drift below a raised 0.42 floor over a
            // 0.40 entry fires nothing in the policy, and the floor would be
            // dead. A TP/SL verdict that DID fire is absorbed (the ladder owns
            // this position's profit-taking discipline — it either acted or is
            // exhausted); every other verdict, and every no-ladder run, keeps
            // the original single ExitRequest below, byte-identical.
            let ladder_owns_verdicts = !self.config.exit_ladder.is_empty();
            if ladder_owns_verdicts && priceable {
                let ladder = self.config.exit_ladder.clone();
                // The bound stop is the position's OWN config view (the
                // per-strategy overlay included) — a holder's ladder patrols
                // at the wide containment width, the same number the verdict
                // functions just judged on.
                let stop_pct = effective_stop_pct(pos_cfg.stop_loss_pct, time_left_sec, pos_cfg);
                let verdict = match &decision {
                    Some(d)
                        if matches!(d.reason, ExitReason::TakeProfit | ExitReason::StopLoss) =>
                    {
                        Some((d.reason, d.use_maker))
                    }
                    _ => None,
                };
                let done = self.ladder_steps_done.entry(pos.id.clone()).or_insert(0);
                let floor = self
                    .ladder_stop_floor
                    .entry(pos.id.clone())
                    .or_insert(Decimal::ZERO);
                let ladder_fired = if let Some(req) = Self::apply_exit_ladder_step(
                    pos,
                    &ladder,
                    done,
                    floor,
                    stop_pct,
                    exit_price,
                    verdict,
                    cfg.maker_first_exit_enabled,
                ) {
                    out.push(req);
                    true
                } else {
                    false
                };
                match &decision {
                    // Absorbed whether the ladder acted or is exhausted.
                    Some(d)
                        if matches!(d.reason, ExitReason::TakeProfit | ExitReason::StopLoss) =>
                    {
                        continue;
                    }
                    // A non-ladder verdict yields this tick to the ladder's own
                    // request if there was one; otherwise it keeps its request.
                    Some(d) if !ladder_fired => out.push(ExitRequest {
                        position_id: pos.id.clone(),
                        reason: d.reason,
                        exit_price,
                        use_maker: d.use_maker,
                        close_shares: None,
                    }),
                    _ => {}
                }
            } else if let Some(d) = decision {
                if priceable {
                    out.push(ExitRequest {
                        position_id: pos.id.clone(),
                        reason: d.reason,
                        exit_price,
                        use_maker: d.use_maker,
                        close_shares: None,
                    });
                } else {
                    // The rule fired but there is no buyer to sell to at any
                    // price we can name — hold, and report the held exit so it
                    // reaches review instead of dying in memory.
                    withheld.push(held_exit(Some(d.reason)));
                }
            } else if escalated {
                // #268 item 2: no rule fired, but this position has not been
                // priceable for longer than the data budget repeated N times.
                // That is not a quiet market — it is a position that cannot
                // act at all, and it is exactly the state the operator had no
                // signal for. Reported through the SAME withheld-exit channel
                // the panel already turns into a risk alert; the throttle
                // (`stop_suppression_repeat_sec`) keeps it from becoming a
                // per-tick storm.
                withheld.push(held_exit(None));
            }
        }
        for ev in withheld {
            self.note_suppressed_stop(ev);
        }
        out
    }

    /// Issue #464: the exit ladder overlay — the ordered `[[risk.ladder]]`
    /// steps over this position's own TP/SL verdict. Runs ONLY when a ladder
    /// is configured and the underlying verdict is a TakeProfit or StopLoss;
    /// the caller then suppresses the verdict's own whole-position request.
    ///
    /// A free function rather than a method on purpose: the exit sweep walks
    /// `self.open` through `iter_mut()`, so a `&mut self` call inside the loop
    /// is a second borrow of the manager. Everything the overlay touches is
    /// passed in as disjoint slices instead (`done`/`floor` are the position's
    /// own entries in the manager's ledgers, default-seeded by the caller).
    ///
    /// Semantics, in order:
    ///
    /// 1. The protective stop is judged FIRST, on the raised floor: the
    ///    effective stop price is `max(policy stop, ladder floor)` — a
    ///    `move_stop_to` only ever lifts the stop, never lowers it. A breach
    ///    closes the ENTIRE remaining position (a stop is not a tranche) and
    ///    the reason stays `StopLoss`.
    /// 2. Otherwise the next un-executed step (`*done`) is the only one that
    ///    may fire — no skipping ahead of an untriggered predecessor, one
    ///    step per tick. When the position's pnl% has reached that step's
    ///    `at_pct`, it sells `close_ratio × current shares`, floored to the
    ///    venue's 0.01 share grid; a floor of zero (a dust remainder) leaves
    ///    the step un-executed and pushes nothing this tick.
    /// 3. A fired step with `move_stop_to` above the current floor raises it
    ///    before the request goes out, and `*done` advances.
    ///
    /// The trigger reads the SAME price the verdict judged on — the
    /// executable bid (`exit_price`) — so the overlay and the policy cannot
    /// disagree about what the market said. `None` = this tick's verdict is
    /// absorbed by the ladder (the fired step was dust, or the next step has
    /// not triggered yet) and no request is produced.
    /// `verdict` is the policy's own (reason, use_maker) when a TP/SL verdict
    /// fired this tick — the step inherits it. On a quiet tick (None) the step
    /// derives its own identity: a profit step IS a take-profit (maker-first
    /// like the policy's), a loss-side step (negative `at_pct`) IS a stop
    /// (taker, always exitable).
    #[allow(clippy::too_many_arguments)]
    fn apply_exit_ladder_step(
        pos: &mut OpenPosition,
        ladder: &[crate::arbitration::LadderStep],
        done: &mut usize,
        floor: &mut Decimal,
        stop_pct: Decimal,
        exit_price: Decimal,
        verdict: Option<(ExitReason, bool)>,
        maker_first: bool,
    ) -> Option<ExitRequest> {
        // (1) The raised stop: policy's effective stop price vs. the ladder's
        // absolute floor, compared at PRICE so the two sources cannot drift.
        let policy_stop_price = pos.entry_price * (Decimal::ONE - stop_pct / Decimal::ONE_HUNDRED);
        let stop_price = (*floor).max(policy_stop_price);
        if pnl_pct(exit_price, pos.entry_price) <= -stop_pct || exit_price <= stop_price {
            tracing::info!(
                position = %pos.id,
                token = %pos.token_id,
                stop_price = %stop_price,
                bid = %exit_price,
                "exit ladder stop floor breached: closing the remaining position"
            );
            return Some(ExitRequest {
                position_id: pos.id.clone(),
                reason: ExitReason::StopLoss,
                exit_price,
                use_maker: false,
                close_shares: None,
            });
        }

        // (2) The next un-executed step. Steps before it already fired; a
        // later step never jumps the queue.
        let Some(step) = ladder.get(*done) else {
            // The ladder is exhausted; the verdict that woke this tick is
            // absorbed (the ladder owns this position's TP/SL discipline).
            return None;
        };
        let pnl = pnl_pct(exit_price, pos.entry_price);
        let triggered = if step.at_pct >= Decimal::ZERO {
            pnl >= step.at_pct
        } else {
            pnl <= step.at_pct
        };
        if !triggered {
            return None;
        }

        // (3) The tranche: close_ratio of the shares still held, floored to
        // the venue grid so the SELL can never oversell. A dust remainder
        // holds the step open — it re-arms on the next tick (the grid can
        // only round down, so a step that floors to zero today stays zero
        // until shares change).
        let tranche = floor_to_grid(pos.shares * step.close_ratio);
        if tranche <= Decimal::ZERO {
            tracing::warn!(
                position = %pos.id,
                step = *done,
                close_ratio = %step.close_ratio,
                shares = %pos.shares,
                "exit ladder step floored to an untradeable tranche; held open"
            );
            return None;
        }

        // (4) The step's stop raise, recorded BEFORE the request so a failure
        // downstream cannot leave the floor behind the price that armed it.
        if let Some(raise) = step.move_stop_to
            && raise > *floor
        {
            *floor = raise;
        }
        let (reason, use_maker) = verdict.unwrap_or_else(|| {
            if step.at_pct >= Decimal::ZERO {
                (ExitReason::TakeProfit, maker_first)
            } else {
                (ExitReason::StopLoss, false)
            }
        });
        let step_index = *done;
        *done += 1;
        tracing::info!(
            position = %pos.id,
            step = step_index,
            at_pct = %step.at_pct,
            close_ratio = %step.close_ratio,
            close_shares = %tranche,
            move_stop_to = ?step.move_stop_to,
            "exit ladder step fired"
        );
        Some(ExitRequest {
            position_id: pos.id.clone(),
            reason,
            exit_price,
            use_maker,
            close_shares: Some(tranche),
        })
    }

    /// Record a withheld protective stop, throttled per position so a wick that
    /// persists for minutes does not become a per-tick alert storm. The first
    /// occurrence is always recorded.
    fn note_suppressed_stop(&mut self, ev: SuppressedStopEvent) {
        let gap_ms = self.config.stop_suppression_repeat_sec.max(0) * 1000;
        if let Some(&last) = self.last_suppression_at.get(&ev.position_id)
            && gap_ms > 0
            && ev.now_ms - last < gap_ms
        {
            return;
        }
        self.last_suppression_at
            .insert(ev.position_id.clone(), ev.now_ms);
        self.suppressed_stop_count += 1;
        self.suppressed_stops.push(ev);
    }

    /// Take the withheld-stop reports accumulated since the last drain. The
    /// kernel turns each one into a `RiskAlert`, so the suppression reaches the
    /// panel and the audit trail instead of dying in memory.
    pub fn drain_suppressed_stops(&mut self) -> Vec<SuppressedStopEvent> {
        std::mem::take(&mut self.suppressed_stops)
    }

    /// Issue #390 (replay nofast): the earliest virtual instant the exit
    /// ladder MUST run again for its verdicts to stay byte-identical to the
    /// every-tick pass — the earliest open position's expiry (the deadline
    /// rules TimeExit/ForceExit, the settlement track, and the
    /// effective-stop ladder are all functions of time-to-expiry). The
    /// throttle beats and the counter ratchets are bounded by the caller's
    /// own report beat; dirty tokens wake the pass through the caller's set.
    /// `None` with no open positions.
    pub fn next_exit_deadline_ms(&self, now_ms: i64) -> Option<i64> {
        self.open
            .iter()
            .map(|p| p.expires_at_ms)
            .filter(|t| *t > now_ms)
            .min()
    }

    /// Issue #390 (replay scheduling): the earliest clock at which the next
    /// maintenance run MUST observe something new — the earlier of the next
    /// position expiry and the next report beat (the 1 s grid that keeps the
    /// every-1 s alert stamps the every-tick pass produced). A quiet gap
    /// between this clock and the previous run re-observes the previous run's
    /// inputs, so the driver may jump the tick grid straight here without
    /// changing any output: the counter ratchets backfill over the whole gap,
    /// and nothing else in the maintenance pass reads the elapsed wall.
    /// `None` with no open positions.
    pub fn next_maintenance_deadline_ms(&self, now_ms: i64, report_beat_ms: i64) -> Option<i64> {
        self.open
            .iter()
            .map(|p| p.expires_at_ms)
            .chain(std::iter::once(now_ms.saturating_add(report_beat_ms)))
            .filter(|t| *t > now_ms)
            .min()
    }

    /// How many suppressed stops have been reported in this process (panel counter).
    pub fn suppressed_stop_count(&self) -> u64 {
        self.suppressed_stop_count
    }
}
