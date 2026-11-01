use super::*;

impl Core {
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
    pub(crate) fn sync_settlement_watch(&mut self, now_ms: i64) {
        // Replay fast path: the watch list only changes when the position set
        // changes, or when the earliest open position's expiry passes (that is
        // the instant `track_markets` would first admit the market). Between
        // those, the scan re-derives the same list — skip it.
        if self.replay_sched()
            && !self.fast_path.fast_positions_dirty
            && now_ms < self.fast_path.fast_next_expiry_ms
        {
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
            self.fast_path.fast_positions_dirty = false;
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
            self.fast_path.fast_next_expiry_ms = all
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
    pub(crate) fn simulate_redemptions(&mut self, now_ms: i64) {
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
    pub(crate) fn simulate_merges(&mut self, now_ms: i64) {
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
    pub(crate) fn dry_resolution(
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
    pub(crate) fn drive_settlement(&mut self, now_ms: i64) {
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
    pub(crate) fn alert_if_settlement_blind(&mut self, now_ms: i64) {
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
    pub(crate) fn run_expiry_fallback_sweep(&mut self, now_ms: i64) {
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
    pub(crate) fn credit_expiry_fallback_cash(
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
        if let Err(e) = crate::runtime_config::append_manual_op(
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
    pub(crate) fn recover_settlements(&mut self) -> usize {
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
    pub(crate) fn settlement_view(&self, as_of_ms: i64) -> serde_json::Value {
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
