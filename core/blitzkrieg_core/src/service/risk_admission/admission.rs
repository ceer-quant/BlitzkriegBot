use super::*;

impl Core {
    // ── Orders ─────────────────────────────────────────────────────────────
    pub fn place(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        let outcome = self.place_inner(req, maker_timeout_ms, now_ms, true)?;
        Ok((outcome.order_id, outcome.status))
    }

    /// [`Core::place`] with the full outcome: the id, the status AND — when the
    /// kernel itself refused the leg after accepting it (a dry/read-only taker
    /// the book cannot fill) — the structured reason for that refusal.
    ///
    /// `orders.place` answers from this (#180) so a caller is never handed a
    /// bare `REJECTED`; everything in-tree keeps the tuple shape, which is what
    /// the strategy/engine paths use.
    pub fn place_outcome(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<PlaceOutcome> {
        self.place_inner(req, maker_timeout_ms, now_ms, true)
    }

    /// Submit a leg that COMPLETES an already-open position rather than starting
    /// a new one — the maker→taker escalation of the same entry.
    ///
    /// The entry gates (`can_open`, breaker) exist to stop a SECOND position on
    /// an asset. When the maker leg filled only partially, the position already
    /// exists, so `can_open` rejects the escalated remainder with "Already in
    /// {asset}" and the leg is silently dropped: the venue-driven partial fill
    /// stays half-filled forever. A LIVE-only defect — dry maker fills are always
    /// full fills. The risk gate still applies to the escalated notional.
    pub(crate) fn place_escalated(
        &mut self,
        req: OrderRequest,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        let outcome = self.place_inner(req, 0, now_ms, false)?;
        Ok((outcome.order_id, outcome.status))
    }

    /// Every refusal this kernel makes about an order funnels through here, so
    /// the one error slot (#180) is updated whichever gate said no — and so the
    /// slot can never be skipped because one path forgot to record.
    pub(crate) fn place_inner(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<PlaceOutcome> {
        let outcome = self.place_gated(req, maker_timeout_ms, now_ms, entry_gates);
        if let Err(e) = &outcome {
            self.note_error(e, now_ms);
        }
        // Replay fast path: a placed order is a live order — the escalation
        // sweep and the audit input changed.
        if outcome.is_ok() && self.replay_sched() {
            self.fast_path.fast_orders_dirty = true;
        }
        outcome
    }

    /// E28 (§9.2, G5): a CLOSE is inventory-bound — the open position for a
    /// token belongs to exactly one account, and a close order naming another
    /// account would settle money into a book that never paid for the shares.
    /// Refused, never re-routed: re-routing would be the implicit-ledger
    /// accident (§9.3) under a different name. Opens are unaffected (a BUY
    /// commits its own book's cash); an exit for a token with NO open
    /// position anywhere passes here and is handled by the fill projection
    /// as before. The judgment is per-account first: the REQUEST'S OWN
    /// account holding the token is the legitimate close (the fill lands on
    /// that account's row); the refusal fires only when the request names an
    /// account that holds NOTHING while the token sits in a SIBLING book —
    /// that order would settle into the wrong book.
    pub(crate) fn refuse_cross_account_close(&self, req: &OrderRequest) -> CoreResult<()> {
        if req.side != Side::Sell || !crate::risk::is_close_intent(&req.internal_key) {
            return Ok(());
        }
        let own = self
            .positions
            .open_positions()
            .iter()
            .any(|p| p.token_id == req.token_id && p.account_id == req.account_id);
        if own {
            return Ok(());
        }
        if let Some(p) = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.token_id == req.token_id)
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "cross-account close refused: position {} on token {} belongs to \
                     account `{}`, the order names `{}`",
                    p.id, p.token_id, p.account_id, req.account_id
                ),
            ));
        }
        Ok(())
    }

    /// Account↔venue binding (first batch): an order whose token belongs to a
    /// DIFFERENT venue's live round market than the account is pinned to is
    /// refused, naming both venues. Untagged on either side passes — an
    /// unbound account (`venue == ""`, the zero-config default) trades
    /// anywhere, and an order whose token belongs to no labelled market (the
    /// engine down, a legacy corpus) carries no venue evidence to judge. The
    /// venue read is the ORDER'S EXECUTION VENUE (`execution_venue_of`):
    /// a shared token carries one row per venue, and judging on a token-only
    /// read alone would refuse on the ambiguity (or pick the wrong row) —
    /// the account's binding names its own row, the same admission blindness
    /// the engine's `scope_admits_token` resolves.
    pub(crate) fn refuse_cross_venue_order(&self, req: &OrderRequest) -> CoreResult<()> {
        let Some(account) = self.accounts.account(&req.account_id) else {
            // Unknown accounts are refused by the ledger lookup right after;
            // nothing venue-shaped to check here.
            return Ok(());
        };
        let bound = account.venue.as_str();
        if bound.is_empty() {
            return Ok(());
        }
        let token_venue = self.execution_venue_of(req.account_id.as_str(), &req.token_id);
        if token_venue.is_empty() || token_venue == bound {
            return Ok(());
        }
        Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!(
                "cross-venue order refused: account `{}` is bound to venue `{bound}` but the \
                 order's token {} belongs to venue `{token_venue}`",
                req.account_id, req.token_id
            ),
        ))
    }

    /// The account's SUBSCRIBED round-duration families — the set of
    /// (venue, duration_sec) pairs this account is allowed to trade — as the
    /// admission gate sees it. THIS PR (B1c) the account book carries no
    /// subscription field (accounts.toml has no `rounds` key; B1d adds it),
    /// so the set is EMPTY for every account: an empty set means "no declared
    /// restriction", and the gate admits everything — today's behaviour,
    /// byte-identical. B1d assembles the real set here (the comment above the
    /// return is the single seam); hot management (subscribe/unsubscribe at
    /// runtime) lands in B2.
    pub(crate) fn account_duration_subscriptions(
        &self,
        account_id: &AccountId,
    ) -> HashSet<(String, i64)> {
        // Test override first (the gate's teeth must stay constructible
        // without a config surface).
        #[cfg(test)]
        if let Some(subs) = &self.test_duration_subscriptions {
            return subs.clone();
        }
        // B1d 装配：真订阅集 = 账户族声明 rounds × venue 绑定。每个声明的
        // duration 与该账户绑定的 venue 组成一对；未绑 venue（空）账户的
        // 声明作用于所有 venue —— 一行 duration 膨胀为当前 live 市场里该
        // duration 的每个 venue（抖动的 venue 集合由 live 市场承载，声明
        // 本身保持 venue 无关）。空 rounds = 未声明任何限制 ⇒ 空表 ⇒ 门
        // 全放行（回放/单族部署的逐字节不变量）。热管理（增族/移族）
        // 落在 B2。
        let Some(account) = self.accounts.account(account_id) else {
            return HashSet::new();
        };
        if account.rounds.is_empty() {
            return HashSet::new();
        }
        let bound_venue = account.venue.as_str();
        let mut out = HashSet::new();
        if !bound_venue.is_empty() {
            for &dur in &account.rounds {
                out.insert((bound_venue.to_string(), dur));
            }
            return out;
        }
        // Unbound: the declaration speaks for every venue the live book
        // currently labels with each declared duration. A venue with no
        // live market at that duration gets no pair — nothing to admit
        // there anyway.
        if let Some(e) = self.engine.as_ref() {
            for m in e.scanner().markets() {
                if m.round_duration_sec > 0 && account.rounds.contains(&m.round_duration_sec) {
                    out.insert((m.venue.clone(), m.round_duration_sec));
                }
            }
        }
        out
    }

    /// Duration-family admission gate (B1c): an order whose (venue, duration)
    /// family is NOT in the account's subscribed family set is refused with
    /// the AccountLimit vocabulary — the same admission dimension family as
    /// `refuse_cross_venue_order` (which binds the venue axis; this binds the
    /// duration axis on top), NOT a new public contract hop (#559/#568's
    /// untouched list is about new hops, not about extending the account
    /// admission chain's existing dimensions).
    ///
    /// Exemptions copy the frozen-gate precedent (the #174 hatch):
    ///  - CLOSE intents always pass — a position admitted when the family was
    ///    subscribed must never be trapped by a later unsubscribe;
    ///  - an account with NO declared family set (the zero-config / pre-B1d
    ///    shape, and today's always-shape) admits everything — an empty set
    ///    is "no restriction declared", not "subscribed to nothing";
    ///  - an order whose token belongs to no labelled live market (the engine
    ///    down, a legacy corpus) carries no family evidence to judge, so it
    ///    passes — whatever refuses it must be another gate with its own
    ///    reason (same stance the venue guard takes for untagged tokens).
    ///
    /// Placement: the `place_gated` chain calls this right AFTER
    /// `refuse_cross_venue_order` and BEFORE the risk/equity gates, so a
    /// family refusal surfaces before notional is reserved against the
    /// ledger. The call convention, the order and the semantics of every
    /// existing gate are untouched.
    pub(crate) fn refuse_unsubscribed_duration(&self, req: &OrderRequest) -> CoreResult<()> {
        // The #174 hatch: closing intents are never family-gated.
        if crate::risk::is_close_intent(&req.internal_key) {
            return Ok(());
        }
        let subs = self.account_duration_subscriptions(&req.account_id);
        if subs.is_empty() {
            // No declared subscription set ⇒ no restriction ⇒ admit. Today's
            // behaviour for every account until B1d wires the real set.
            return Ok(());
        }
        // The venue read is the ORDER'S EXECUTION VENUE: a shared token
        // carries one row per venue, and the placing account's binding names
        // its own row — a token-only read could not pick between the
        // candidates (the same blindness the venue guard above sheds).
        let token_venue = self.execution_venue_of(req.account_id.as_str(), &req.token_id);
        if token_venue.is_empty() {
            // No labelled market, no family evidence — same stance as the
            // venue guard's untagged-token pass.
            return Ok(());
        }
        let duration = self.duration_of_token_for_admission(&req.token_id);
        if subs.contains(&(token_venue.clone(), duration)) {
            return Ok(());
        }
        Err(CoreError::new(
            CoreErrorCode::AccountLimit,
            format!(
                "unsubscribed round family: account `{}` has no (venue, duration) subscription \
                 for (`{token_venue}`, {duration}s) — order's token {} refused (close intents \
                 are exempt)",
                req.account_id, req.token_id
            ),
        ))
    }

    /// The round-length family a token's market declares, in seconds, as the
    /// admission gate reads it. `0` = undeclared: the market carries no
    /// family fact (legacy corpus / the engine down). The gate refuses on the
    /// exact declared value — a `0` can never match a subscribed (venue,
    /// duration) pair, but the unlabelled-market exemption above already
    /// admitted the empty-venue case; a venue-tagged market without a
    /// duration declaration is a B1d concern (the subscription writer must
    /// stamp durations on every row it lists).
    pub(crate) fn duration_of_token_for_admission(&self, token_id: &str) -> i64 {
        self.engine
            .as_ref()
            .and_then(|e| e.market_for_token(token_id))
            .map(|m| m.round_duration_sec)
            .unwrap_or(0)
    }

    /// Test seam for the B1c gate's teeth: stages a subscription set that
    /// [`Self::account_duration_subscriptions`] returns for every account.
    /// The real per-account assembly is B1d's (the Account struct gains its
    /// rounds field there); this override exists so the gate's two verdicts
    /// (subscribed / unsubscribed) are provable TODAY, red-green, before any
    /// config surface exists. Never compiled outside tests — production reads
    /// the constant empty set.
    #[cfg(test)]
    pub(crate) fn stage_duration_subscriptions_for_test(&mut self, subs: HashSet<(String, i64)>) {
        self.test_duration_subscriptions = Some(subs);
    }

    /// #774 test seam: stage a fill that reaches the OME after the escalation
    /// sweep has taken its `size - filled_size` snapshot, but before it cancels
    /// the maker — the venue-race window the sweep's fresh re-read exists for.
    /// A single-threaded unit test has no interleaving point there, so the
    /// staged fill is applied at that seam by `run_escalation_sweep`. Never
    /// compiled outside tests.
    #[cfg(test)]
    pub(crate) fn stage_escalation_snapshot_fill_for_test(&mut self, fill: Fill) {
        self.test_escalation_snapshot_fill = Some(fill);
    }

    pub(crate) fn place_gated(
        &mut self,
        mut req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<PlaceOutcome> {
        // An intent the venue just refused waits out its backoff locally —
        // re-POSTing at full tick rate is exactly the rejection storm this
        // guards against. Manual attempts are only paused while the backoff
        // window is running (≤ 30s), never permanently abandoned.
        if let Some(cd) = self.place_cooldowns.get(&req.internal_key)
            && now_ms < cd.next_ok_ms
        {
            return Err(crate::model::CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::RiskRejected,
                format!(
                    "intent {} in rejection cooldown ({}ms left): {}",
                    req.internal_key,
                    cd.next_ok_ms - now_ms,
                    cd.last_error
                ),
            ));
        }
        // E28 Gate 2 (§9.1): the account's posture gates the place BEFORE the
        // risk gates consult equity — only `Active` places new entries; closes
        // pass every posture except `ReadOnly` (a freeze must never trap a
        // position). This is the submission-path enforcement point; the
        // arbitration pipeline carries the same check earlier for strategy
        // intents (defense in depth, one vocabulary: ACCOUNT_LIMIT).
        self.accounts
            .permits_order(&req.account_id, is_close_intent(&req.internal_key))?;
        // Account-scoped kill switch: a frozen account refuses NEW orders at
        // the submission gate (closes pass — the #174 hatch, per account).
        // Manual-orders enforcement point; strategy entries were already
        // dropped in the engine (the frozen-scope filter).
        self.account_freeze_permits(&req)?;
        // G5: a close settles into the book that PAID for the shares — a
        // close order naming another account is refused, never re-routed.
        self.refuse_cross_account_close(&req)?;
        // Account↔venue binding (first batch): an account pinned to venue V
        // may not trade a token that belongs to venue W's round market —
        // money must not leave through a book that was never wired to that
        // venue. Unbound accounts (venue == "") and orders whose token
        // belongs to no labelled live market pass untouched, so the 0.2
        // single-plugin behaviour is bit-identical. The account.switch
        // linkage (switching accounts switching the default plugin) is left
        // for the next batch — the "switch" semantics (session vs process
        // level) are still ambiguous and must not be guessed here.
        self.refuse_cross_venue_order(&req)?;
        // Duration-family admission (B1c): the same account-admission chain
        // extended to the (venue, duration) axis — an order of a round-length
        // family the account has not subscribed is refused (AccountLimit),
        // closes exempt. Until B1d wires the real subscription set no account
        // declares one, so this admits everything today — the hook is placed
        // now, in position, and takes its teeth from B1d's assembly.
        self.refuse_unsubscribed_duration(&req)?;
        // #202: the equity-relative per-order cap is a percentage of the account
        // AT SUBMISSION, so the gate is handed the live balance rather than a
        // remembered copy of it. E28: the account is the one the request names —
        // an unknown account is refused, never implicitly created.
        self.risk
            .check_with_equity(&req, self.accounts.ledger_of(&req.account_id)?.balance())?;
        // #461 Fix 2/4: the systemic nine and the pair-cost cap judge EVERY
        // entry here — manual `orders.place` included, so there is no
        // bypass (the arbitration pipeline runs the same verdict earlier for
        // strategy intents; this is the submission-path enforcement point).
        // `judge_entry`'s Reduce arm shrinks the order in place, exactly as
        // the pipeline's Modification::SizeReduced does for strategy entries;
        // its Reject arm refuses with the gate's own vocabulary.
        if req.side == Side::Buy {
            let limits = self.risk.systemic_snapshot();
            if limits.any_armed() {
                let facts = self.systemic_facts(&req.asset, now_ms);
                // The bound stop is the discipline that will ACTUALLY fire:
                // a hold-to-settlement strategy's ladder walks the wide
                // containment width, so the cap judges on that number too
                // (the same doctrine the PHYSICS projection binds).
                let holder = self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.strategy_holds_to_settlement(&req.strategy));
                let bound_stop_pct = if holder {
                    crate::exit_policy::holder_effective_stop_pct(0, &self.config.positions.exit)
                } else {
                    crate::exit_policy::effective_stop_pct(
                        self.config.positions.exit.stop_loss_pct,
                        0,
                        &self.config.positions.exit,
                    )
                };
                match crate::risk::limits::judge_entry(
                    limits,
                    &facts,
                    req.price,
                    req.size,
                    bound_stop_pct,
                ) {
                    crate::risk::limits::EntryJudgment::Pass => {}
                    crate::risk::limits::EntryJudgment::Reduce { approved, limit } => {
                        tracing::info!(
                            strategy = %req.strategy,
                            requested = %req.size,
                            approved = %approved,
                            limit,
                            "systemic: entry shrunk at the submission gate"
                        );
                        req.size = approved;
                    }
                    crate::risk::limits::EntryJudgment::Reject { detail, .. } => {
                        return Err(CoreError::new(CoreErrorCode::RiskRejected, detail));
                    }
                }
            }
            // The pair-cost cap: a BUY whose condition already has a
            // counterpart position may not push the pair's PER-SHARE cost
            // past the $1.00 a share-pair redeems (a fresh pair is the
            // strategy's own sub-$1.00 decision; this gate covers the
            // top-up/chase paths). The worst held leg prices the pair: a
            // position's per-share basis is `cost_usd / shares`, and one
            // share of UP plus one share of DOWN redeems exactly $1.
            // 跨账户双腿：分母只看本账户的对手腿（同一账本内赎回 $1，
            // 见 counterpart_pair_basis 的裁决注释）。
            if let Some(paid) = self.counterpart_pair_basis(
                &req.condition_id,
                &req.asset,
                &parse_direction(&req.direction),
                &req.account_id,
            ) {
                let total = paid + req.price;
                if total > Decimal::ONE {
                    return Err(CoreError::new(
                        CoreErrorCode::RiskRejected,
                        format!(
                            "pair-cost cap: counterpart leg basis {paid}/share + buying at {} = \
                             {total} > $1.00 a pair redeems — the merge would lock in a loss",
                            req.price
                        ),
                    ));
                }
            }
        }

        // Entry gates apply to opening BUY orders only; exits (SELL) are never
        // blocked by capacity, breaker or cooldowns.
        if entry_gates && req.side == Side::Buy {
            // A failing accounting audit blocks NEW entries (issue #189). It is
            // deliberately not the kill switch: that also blocks closes (#174), so
            // a drifted ledger would trap the bot in whatever it holds. Only the
            // opening side is refused, and only until an audit passes again.
            self.audit_entry_gate()?;
            // KI-10: only the ORDER'S OWN strategy's streak can block it — a
            // losing leg no longer vetoes every other strategy's entries.
            if self.breaker.is_halted(&req.strategy, now_ms) {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!(
                        "breaker active until {} for strategy {}",
                        self.breaker.halted_until_ms(&req.strategy),
                        req.strategy
                    ),
                ));
            }
            let direction = parse_direction(&req.direction);
            if let Err(reason) = self.positions.can_open(
                req.account_id.as_str(),
                Some(&req.asset),
                Some(direction),
                now_ms,
            ) {
                // PAIR-COMPLETION exemption (hold-to-settlement strategies
                // only): the one-position-per-asset gate would reject the
                // second leg of a complete set, but UP + DOWN of the SAME
                // condition is market-neutral — the pair pays $1 at settlement
                // regardless of the outcome, so it is not a doubling of
                // exposure. The override is deliberately narrow: only the
                // specific "Already in {asset}" rejection, only for a strategy
                // that declared hold-to-settlement, only against a position
                // it already holds on the SAME condition — either the
                // OPPOSITE leg (the second leg of a fresh pair) or a TOP-UP
                // of a partially-filled leg toward its counterpart (the
                // gap-fill loop; the opened position itself is the evidence
                // of the incomplete pair). Same-direction stacking across
                // conditions and multi-condition exposure remain rejected,
                // and every other can_open reason (capacity, daily loss,
                // cooldowns) still applies.
                let pair_completion = reason.starts_with("Already in")
                    && self
                        .engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(&req.strategy))
                    && self.positions.open_positions().iter().any(|p| {
                        p.asset == req.asset
                            && p.condition_id == req.condition_id
                            && p.strategy == req.strategy
                            && (p.direction != direction || p.shares < req.size)
                    });
                if !pair_completion {
                    return Err(CoreError::new(CoreErrorCode::RiskRejected, reason));
                }
                tracing::info!(
                    strategy = %req.strategy,
                    asset = %req.asset,
                    direction = req.direction,
                    "pair-completion entry allowed past the one-per-asset gate (hold-to-settlement)"
                );
            }
        }

        let id = self.new_order_id();
        let buy = req.side == Side::Buy;
        let req_account = req.account_id.clone();
        let req_token = req.token_id.clone();

        // Reserve BUY notional up front (prevents over-commitment). E28: the
        // reservation commits cash in the REQUEST'S OWN book.
        if buy {
            self.accounts
                .get_mut(&req.account_id)?
                .reserve(&id, req.price * req.size)?;
        }

        // #351 order lifecycle: the decision is THIS instant; the leg arrives
        // at the venue `taker_latency_ms` later and its report lands
        // `taker_rtt_ms` after that. Zero dials = all three collapse to
        // `now_ms`, the historic immediate-fill behaviour.
        let execute_at = now_ms.saturating_add(self.config.fill_model.taker_latency_ms.max(0));
        let report_at = execute_at.saturating_add(self.config.fill_model.taker_rtt_ms.max(0));

        if let Err(e) = self.ome.submit(SubmitParams {
            decision_at_ms: now_ms,
            execute_at_ms: execute_at,
            report_at_ms: report_at,
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
            maker_timeout_ms,
            // B1b: the order's round-length family, resolved three-state off
            // the token's market and the global grid.
            round_duration_sec: self.round_family_of_token(&req_token),
        }) {
            // #537: same rollback contract as the pre-submit path — a submit
            // refusal must not strand the notional it just reserved.
            if buy && let Some(l) = self.accounts.get_mut(&req_account).ok() {
                l.release(&id);
            }
            return Err(e);
        }
        let rejection = self.place_after_submit(&id, maker_timeout_ms, now_ms)?;

        let status = self
            .ome
            .get(&id)
            .map(|o| o.status)
            .unwrap_or(OrderStatus::Pending);
        self.emit_order(&id);
        Ok(PlaceOutcome {
            order_id: id,
            status,
            rejection,
        })
    }

    /// The price a taker leg must state to guarantee a full fill of `size` on
    /// `token`, walked off the mirrored book (best levels first, no worse
    /// than `limit`), plus the volume-weighted price the walk would fill at.
    /// `None` when the book cannot cover the size within `limit` — exactly
    /// what a live FOK would be killed for — or when the book is STALE
    /// (#462): the same `max_book_age_sec` budget the exit ladder prices
    /// under also gates what this walk may authorize. A chase priced off a
    /// quote the feed stopped updating minutes ago is not a fill guarantee,
    /// it is a blind limit; the walk refuses it the same way. Books with no
    /// receive time (`received_ms <= 0`, synthetic fixtures) keep the
    /// historical "age unknown" exemption so dry tests still walk.
    pub(crate) fn marketable_walk(
        &self,
        side: Side,
        token: &str,
        size: Decimal,
        limit: Decimal,
        now_ms: i64,
    ) -> Option<(Decimal, Decimal)> {
        let book = self.books.get(token)?;
        if book.received_ms > 0
            && book.received_ms
                < now_ms.saturating_sub(self.config.positions.exit.max_book_age_sec * 1_000)
        {
            return None;
        }
        book.walk_marketable(side, limit, size)
    }

    /// The chase request's provenance fields, read off the token's live round
    /// market (the same engine registry the panels' Venue column and the
    /// cross-venue binding use): the round slot the missing leg belongs to,
    /// its asset, and its leg label. `None` = the token belongs to no live
    /// market right now (the engine down, a legacy corpus) — the chase caller
    /// falls back to the sweep's own defaults instead of guessing a venue
    /// fact it cannot see.
    pub(crate) fn books_round_slot(&self, token: &str) -> Option<i64> {
        self.engine.as_ref().and_then(|e| e.round_slot_of(token))
    }

    pub(crate) fn books_asset(&self, token: &str) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|e| e.asset_of(token))
            .map(|a| a.to_string())
    }

    pub(crate) fn books_direction(&self, token: &str) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|e| e.direction_of(token))
            .map(|d| d.as_str().to_string())
    }

    /// Why a dry/read-only taker leg could not be filled, as the structured
    /// error the submitter gets back (#180).
    ///
    /// Two different causes reach this point — nothing crosses the limit, or not
    /// enough size rests at or inside it — and they are different operational
    /// facts (a quote that moved vs a book too thin for our size), so each gets a
    /// stable leading token a log search, the panel and a gate can match on. Both
    /// carry `WOULD_CROSS`: that is exactly the code a live FOK is killed under
    /// (the venue adapter maps its "no match" / "cross" / "post-only" errors to
    /// it), and a refusal must not invent a second error model.
    pub(crate) fn taker_refusal(&self, order: &TrackedOrder, book: &Book) -> CoreError {
        let (crosses, depth) = book.crossing_depth(order.side, order.price);
        let best = match order.side {
            Side::Buy => book.best_ask().map(|p| format!("best ask {p}")),
            Side::Sell => book.best_bid().map(|p| format!("best bid {p}")),
        }
        .unwrap_or_else(|| {
            format!(
                "no {} rests",
                match order.side {
                    Side::Buy => "ask",
                    Side::Sell => "bid",
                }
            )
        });
        let message = match (order.side, crosses) {
            (Side::Buy, false) => format!(
                "taker_no_crossing_liquidity: no ask at or inside the {} buy limit ({best}; token {}, size {})",
                order.price, order.token_id, order.size
            ),
            (Side::Sell, false) => format!(
                "taker_no_crossing_liquidity: no bid at or inside the {} sell limit ({best}; token {}, size {})",
                order.price, order.token_id, order.size
            ),
            (Side::Buy, true) => format!(
                "taker_insufficient_depth: only {depth} of {} shares rest at or inside the {} buy limit ({best}; token {})",
                order.size, order.price, order.token_id
            ),
            (Side::Sell, true) => format!(
                "taker_insufficient_depth: only {depth} of {} shares rest at or inside the {} sell limit ({best}; token {})",
                order.size, order.price, order.token_id
            ),
        };
        CoreError::new(CoreErrorCode::WouldCross, message)
    }

    /// The dry/read-only half of submission: settle the leg locally, since there
    /// is no venue to report a fill. Returns the kernel-side refusal when the leg
    /// was rejected here, so the submitter learns WHY (#180).
    pub(crate) fn place_after_submit(
        &mut self,
        id: &str,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<Option<CoreError>> {
        let order =
            self.ome.get(id).cloned().ok_or_else(|| {
                CoreError::new(CoreErrorCode::Internal, "order missing after submit")
            })?;

        match self.config.mode {
            // Dry and ReadOnly share the settle path: neither has a venue that
            // can report a fill, so waiting would leave the order Pending
            // forever and the ledger would never move. What separates them is
            // egress, not accounting — see `Mode::readonly`.
            Mode::Dry | Mode::ReadOnly => {
                // #351: an in-flight taker has NOT reached the venue yet — it
                // stays Pending (the same shape a LIVE order has until the
                // venue acks) and turns terminal only at its report time.
                let model = self.config.fill_model;
                let inflight = order.mode == FillPolicy::Taker
                    && (model.taker_latency_ms > 0 || model.taker_rtt_ms > 0);
                if !inflight {
                    self.ome.mark_live(id, now_ms)?;
                }
                let mut rejection = None;
                match order.mode {
                    FillPolicy::Taker => {
                        // #351: with the latency dials engaged the leg is IN
                        // FLIGHT — it matches at `execute_at_ms` against the
                        // book as it stands THEN (never the decision-time
                        // quote) and reports at `report_at_ms`. Zero dials
                        // keep the historic immediate fill below, bit-for-bit.
                        let model = self.config.fill_model;
                        if model.taker_latency_ms > 0 || model.taker_rtt_ms > 0 {
                            let arrive = order.execute_at_ms.max(now_ms);
                            self.inflight_takers.push(InflightTaker {
                                order_id: id.to_string(),
                                execute_at_ms: arrive,
                                report_at_ms: order.report_at_ms.max(arrive),
                                matched_price: None,
                                rejected_msg: None,
                            });
                        } else {
                            // Fill like a live FOK: walk the opposing side's resting
                            // levels, best first, no worse than the order's own
                            // limit, and fill at the volume-weighted price. A book
                            // that does not cross or cannot cover the size is what
                            // the venue would kill the order for — reject with the
                            // same economics instead of inventing a fill at a price
                            // nobody was offering. The walk prices the VISIBLE
                            // book; the fill model's slippage dial prices the
                            // impact of our own size beyond it (identity by
                            // default → unchanged economics).
                            let book = self.books.get(&order.token_id).cloned().unwrap_or_default();
                            match book.walk_marketable(order.side, order.price, order.size) {
                                Some((vwap, _)) => {
                                    let fill_price =
                                        self.config.fill_model.apply_slippage(order.side, vwap);
                                    // A FOK is all-or-nothing by construction, so it
                                    // is one execution of the whole size — the trade
                                    // id stays the one this path has always used.
                                    let trade_id = format!("{id}:{now_ms}");
                                    self.authoritative_fill(
                                        id, &trade_id, order.size, fill_price, false, now_ms,
                                    )?
                                }
                                None => {
                                    if order.side == Side::Buy {
                                        // E28: the refused taker's reservation is
                                        // released in the ORDER'S OWN book.
                                        if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                                            l.release(id);
                                        }
                                        // E-VA: the virtual hold releases with it
                                        // (the leg never rested).
                                        self.virtual_release_order(&order);
                                    }
                                    self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
                                    // RFC 568 阶段一：死亡事实（taker 拒绝）
                                    // 喂配对簿。
                                    self.pair_on_order_event(
                                        &order.internal_key,
                                        Decimal::ZERO,
                                        true,
                                    );
                                    // The refused FOK never rested: its token may
                                    // be re-attempted on a refreshed book.
                                    if let Some(engine) = self.engine.as_mut() {
                                        engine.release_pending_token_for_strategy(
                                            order.account_id.as_str(),
                                            &order.strategy,
                                            &order.token_id,
                                        );
                                    }
                                    // into the one error slot, reaches Node as an
                                    // Event::Error, lands in the log at error level and
                                    // travels back to the submitter on the
                                    // `orders.place` result. Before this, the reject
                                    // branch emitted an order update and nothing else,
                                    // so `orders.place` answered a bare REJECTED and
                                    // `engine.stats.lastError` stayed empty.
                                    let err = self.taker_refusal(&order, &book);
                                    tracing::error!(
                                        order = id,
                                        token = %order.token_id,
                                        side = ?order.side,
                                        size = %order.size,
                                        limit = %order.price,
                                        reason = %err.message,
                                        "order rejected: taker leg has no fillable liquidity"
                                    );
                                    self.emit_error_at(err.clone(), now_ms);
                                    self.emit_order(id);
                                    rejection = Some(err);
                                }
                            }
                        }
                    }
                    FillPolicy::Maker | FillPolicy::MakerThenTaker => {
                        if order.mode == FillPolicy::MakerThenTaker
                            && let Some(delay) = escalation_delay_ms(
                                maker_timeout_ms,
                                self.config.default_maker_timeout_ms,
                            )
                        {
                            self.ome.set_escalation(id, now_ms + delay)?;
                        }
                        // Fill now if the current book already crosses.
                        self.try_maker_fill(id, now_ms);
                    }
                }
                // `emit_order` after the rejection is recorded, so an event
                // consumer that reads stats on the update sees the reason.
                Ok(rejection)
            }
            Mode::Live => {
                // Pending until the venue adapter confirms; the async layer calls
                // mark_live / feeds user-WS fills. Implemented with the CLOB adapter.
                Ok(None)
            }
        }
    }

    /// Apply one authoritative (cumulative) fill and its ledger effect.
    ///
    /// `maker` is the role this execution actually had, which the dry matcher
    /// knows exactly: it either crossed a resting bid (`false`) or rested and was
    /// hit (`true`). Stating it keeps DRY and LIVE on the same authority — the
    /// execution itself — instead of DRY inferring from policy and LIVE reading
    /// the venue.
    ///
    /// `trade_id` is the execution's own identity, and `cumulative` is the size
    /// this identity has now reported — NOT an increment. Two executions of one
    /// order must therefore carry two different ids (a partial fill followed by
    /// a further partial fill is two trades), which is exactly how a venue
    /// reports them; re-reporting the same id is a revision of what was already
    /// booked, not a second trade (issue #178).
    pub(crate) fn authoritative_fill(
        &mut self,
        id: &str,
        trade_id: &str,
        cumulative: Decimal,
        price: Decimal,
        maker: bool,
        now_ms: i64,
    ) -> CoreResult<()> {
        let (token, side, account_id) = match self.ome.get(id) {
            Some(o) => (o.token_id.clone(), o.side, o.account_id.clone()),
            None => return Err(CoreError::new(CoreErrorCode::UnknownOrder, id)),
        };
        let fill = Fill {
            order_id: id.into(),
            trade_id: Some(trade_id.to_string()),
            token_id: token,
            account_id,
            side,
            price,
            size: cumulative,
            status: FillStatus::Confirmed,
            ts_ms: now_ms,
            tx_hash: None,
            maker: Some(maker),
        };
        if let Some(d) = self.ome.apply_fill(fill, now_ms)? {
            self.apply_delta_effects(d, now_ms);
        }
        // Event-stream completeness: a fill applied HERE is outside `place`, so
        // `place`'s trailing `emit_order` cannot cover it. Without this final
        // update the order log / backtester only ever saw the resting LIVE state
        // (the Fill event alone carries no terminal status), so a cross-filled
        // maker read as still-open to every event consumer.
        self.emit_order(id);
        Ok(())
    }

    /// Fill a resting maker order if the latest book crosses its limit.
    ///
    /// The fill is bounded by the crossing side's DEPTH, not by the order's own
    /// size (issue #183): a maker trades only against the volume that actually
    /// reaches its limit, so an order bigger than the book leaves the rest
    /// resting — partially filled — instead of pretending the whole size
    /// traded. The depth reading is the taker walk's own
    /// ([`crate::sim::Book::marketable_depth`]), so dry cannot hold two
    /// different ideas of how much is available.
    ///
    /// #464: a maker that crosses the book AT SUBMISSION is refused here, not
    /// filled. Live sends a post-only limit and the venue answers WouldCross
    /// (`map_sdk_err`, extensions/polymarket/src/venue.rs) — filling the same
    /// order at its own limit with a zero maker fee in dry handed the
    /// simulation maker economics for what live would have paid a taker fee
    /// to cross. The refusal routes through the SAME live machinery:
    /// `reject_live_result` releases the reservation, and the MakerThenTaker
    /// branch there chases at the book's real price (`escalation_guard` +
    /// `place_escalated`), so both modes converge on one path. A maker that
    /// crosses only LATER (rested first, then hit) is the honest maker fill
    /// and still fills below.
    ///
    /// #351: advance the in-flight taker legs. Runs at every clock advance —
    /// the head of [`Self::engine_on_data`] (BEFORE the incoming book is
    /// mirrored, so a match can never see the event that triggered the drain)
    /// and in [`Self::tick`]. Both call sites observe the invariant that the
    /// mirrored book only holds events ≤ the current clock, so a match is
    /// priced strictly off data the venue could have had at `execute_at_ms`
    /// or earlier — no future data, by construction.
    ///
    /// Stage 1 (match, once per leg at the first drain with
    /// `now >= execute_at_ms`): the FOK walks the CURRENT mirrored book at the
    /// order's decision-time limit; a book that no longer crosses or cannot
    /// cover the size is a kill — exactly what a live venue does to a late
    /// FOK. Stage 2 (report, `now >= report_at_ms`): the fill books with that
    /// instant (ledger/position effects land at report time), or the kill
    /// releases the BUY reservation.
    pub(crate) fn drain_inflight_takers(&mut self, now_ms: i64) {
        if self.inflight_takers.is_empty() {
            return;
        }
        let model = self.config.fill_model;
        let legs = std::mem::take(&mut self.inflight_takers);
        let mut remaining = Vec::new();
        for mut leg in legs {
            // Stage 1: match at the venue.
            if leg.matched_price.is_none() && leg.rejected_msg.is_none() {
                if now_ms < leg.execute_at_ms {
                    remaining.push(leg);
                    continue;
                }
                let order = match self.ome.get(&leg.order_id).cloned() {
                    Some(o) if o.status.is_live() => o,
                    // Cancelled/terminal while in flight: the venue never
                    // filled it (the cancel won the race) — drop the leg.
                    _ => continue,
                };
                let book = self.books.get(&order.token_id).cloned().unwrap_or_default();
                match book.walk_marketable(order.side, order.price, order.size) {
                    Some((vwap, _)) => {
                        leg.matched_price = Some(model.apply_slippage(order.side, vwap));
                    }
                    None => leg.rejected_msg = Some(self.taker_refusal(&order, &book).message),
                }
            }
            // Stage 2: report at the local books.
            if now_ms < leg.report_at_ms {
                remaining.push(leg);
                continue;
            }
            if let Some(price) = leg.matched_price {
                let order = match self.ome.get(&leg.order_id) {
                    // A cancel that won the race between match and report
                    // leaves the order terminal: the venue never reports a
                    // fill for an order it was told to forget.
                    Some(o) if o.status.is_live() => o.clone(),
                    Some(_) => continue,
                    None => continue,
                };
                let trade_id = format!("{}:{}", order.order_id, leg.execute_at_ms);
                if let Err(e) = self.authoritative_fill(
                    &order.order_id,
                    &trade_id,
                    order.size,
                    price,
                    false,
                    leg.report_at_ms,
                ) {
                    tracing::error!(
                        order = %order.order_id,
                        error = %e,
                        "inflight taker fill could not be applied"
                    );
                    self.emit_error_at(e, leg.report_at_ms);
                }
            } else if let Some(msg) = leg.rejected_msg.take() {
                let order = match self.ome.get(&leg.order_id) {
                    Some(o) => o.clone(),
                    None => continue,
                };
                if order.side == Side::Buy {
                    // E28: the killed FOK's reservation is released in the
                    // ORDER'S OWN book.
                    if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                        l.release(&order.order_id);
                    }
                    // E-VA: the virtual hold goes with it.
                    self.virtual_release_order(&order);
                }
                let _ = self.ome.mark_terminal(
                    &order.order_id,
                    OrderStatus::Rejected,
                    leg.report_at_ms,
                );
                // RFC 568 阶段一：死亡事实（in-flight taker 被杀）喂配对簿。
                self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
                let err = CoreError::new(CoreErrorCode::WouldCross, msg);
                tracing::error!(
                    order = %order.order_id,
                    reason = %err.message,
                    "inflight taker killed at execute_at (book no longer crosses)"
                );
                self.emit_error_at(err, leg.report_at_ms);
                self.emit_order(&order.order_id);
            }
        }
        self.inflight_takers = remaining;
    }

    /// Fill a resting maker order if the latest book crosses its limit.
    ///
    /// The fill is bounded by the crossing side's DEPTH, not by the order's own
    /// size (issue #183): a maker trades only against the volume that actually
    /// reaches its limit, so an order bigger than the book leaves the rest
    /// resting — partially filled — instead of pretending the whole size
    /// traded. The depth reading is the taker walk's own
    /// ([`crate::sim::Book::marketable_depth`]), so dry cannot hold two
    /// different ideas of how much is available.
    ///
    /// #464: a maker that crosses the book AT SUBMISSION is refused here, not
    /// filled. Live sends a post-only limit and the venue answers WouldCross
    /// (`map_sdk_err`, extensions/polymarket/src/venue.rs) — filling the same
    /// order at its own limit with a zero maker fee in dry handed the
    /// simulation maker economics for what live would have paid a taker fee
    /// to cross. The refusal routes through the SAME live machinery:
    /// `reject_live_result` releases the reservation, and the MakerThenTaker
    /// branch there chases at the book's real price (`escalation_guard` +
    /// `place_escalated`), so both modes converge on one path. A maker that
    /// crosses only LATER (rested first, then hit) is the honest maker fill
    /// and still fills below.
    pub(crate) fn try_maker_fill(&mut self, id: &str, now_ms: i64) {
        let Some(order) = self.ome.get(id).cloned() else {
            return;
        };
        if !order.status.is_live() || order.filled_size >= order.size {
            return;
        }
        let Some(book) = self.books.get(&order.token_id) else {
            return;
        };
        if !book.crosses(&order) {
            return;
        }
        // #464 (see above): the venue would have refused the resting order —
        // post-only may not cross. Only the SUBMISSION instant is gated, so a
        // maker that rested and is hit by a later tick keeps its fill path
        // untouched (bit-for-bit for every order that did not cross on POST).
        // CLOSE intents are exempt: live refuses their POST too (the
        // WouldCross escalation branch is entry-only), so the honest mirror
        // of live is the exit runner's own reject→backoff→retry loop, not a
        // dry refusal that would strand the position with no closer at all.
        if order.submitted_at_ms == now_ms && !is_close_intent(&order.internal_key) {
            let side_word = if order.side == Side::Buy {
                "buy"
            } else {
                "sell"
            };
            self.reject_live_result(
                id,
                Some(CoreError::new(
                    CoreErrorCode::WouldCross,
                    format!(
                        "post-only order would cross: {side_word} {} @ {} (the venue refuses \
                         this POST; a MakerThenTaker entry escalates to taker on the same book)",
                        order.size, order.price
                    ),
                )),
                now_ms,
            )
            .ok();
            return;
        }
        let depth = book.marketable_depth(order.side, order.price);
        // Fill model (P-1.2), identity by default so this path is unchanged:
        //  - latency: the order cannot be hit before the venue could have it;
        //  - fill probability: a crossing does not guarantee a fill (queue
        //    position) — a deterministic per-order draw keeps replays exact.
        let model = self.config.fill_model;
        if now_ms < model.maker_eligible_at_ms(order.submitted_at_ms) {
            return;
        }
        if !model.maker_fill_wins(&order.order_id) {
            return;
        }
        let remaining = (order.size - order.filled_size).max(Decimal::ZERO);
        let chunk = model.maker_fill_size(&order.order_id, remaining, depth);
        if chunk <= Decimal::ZERO {
            // The crossing side offers nothing to take: the order keeps resting.
            return;
        }
        // A maker still fills at its own resting limit (Node parity); only the
        // SIZE is bounded by the book. Each chunk is its own trade identity, so
        // a later chunk books on top of this one instead of revising it.
        let trade_id = format!("{id}:{now_ms}:{}", order.filled_size + chunk);
        if let Err(e) = self.authoritative_fill(id, &trade_id, chunk, order.price, true, now_ms) {
            // Through the one error slot like every other internal failure: a fill
            // the OME refuses is exactly the kind of thing an operator must see
            // (#180/#184 rather than a bare Event::Error).
            tracing::error!(order = id, error = %e, "maker fill could not be applied");
            self.emit_error_at(e, now_ms);
        }
    }
}
