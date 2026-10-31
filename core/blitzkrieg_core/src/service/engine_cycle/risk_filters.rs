//! Risk/sizing pre-filters, virtual-account quota and the cross-venue
//! arb pre-check (#780): pure move from `service/engine_cycle`.

use super::*;

impl Core {
    /// Portfolio-level exposure cap (E16/#98): the sum of open notional across
    /// ALL strategies plus the incoming entry stays under
    /// `risk.max_open_notional_usd` (0 = disabled).
    /// #461 Fix 3: the systemic aggregates the nine limits judge, with
    /// RESTING exposure counted. Positions carry their cost basis (cash
    /// committed); live BUY orders carry their remaining notional (cash
    /// PROMISED — a resting bid fills at any moment and the exposure cap
    /// must not discover the commitment only after it became a position).
    /// SELL orders commit nothing new (they release inventory), so only the
    /// buy side counts.
    pub(crate) fn systemic_facts(
        &self,
        asset: &str,
        now_ms: i64,
    ) -> crate::risk::limits::SystemicFacts {
        // The systemic COUNT judges live positions only (expired rounds are
        // settlement-dead — same rule as PositionManager::can_open); the
        // exposure sums keep every position: their cost basis is cash
        // actually committed until the venue settles.
        crate::risk::limits::SystemicFacts {
            open_positions: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.expires_at_ms > now_ms)
                .count(),
            open_exposure_usd: self
                .positions
                .open_positions()
                .iter()
                .map(|p| p.cost_usd)
                .chain(
                    self.ome
                        .live_orders()
                        .iter()
                        .filter(|o| o.side == Side::Buy)
                        .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO)),
                )
                .sum(),
            same_asset_exposure_usd: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.asset == asset)
                .map(|p| p.cost_usd)
                .chain(
                    self.ome
                        .live_orders()
                        .iter()
                        .filter(|o| o.side == Side::Buy && o.asset == asset)
                        .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO)),
                )
                .sum(),
            day_realized_usd: self.positions.daily_pnl_total(),
        }
    }

    /// #461 Fix 2: the worst per-share basis ALREADY PAID on the counterpart
    /// leg of this condition (None when no counterpart is held). One UP share
    /// plus one DOWN share redeems exactly $1.00, so the pair economics are
    /// per-share: `counterpart_basis/share + this leg's price ≤ 1.00`. The
    /// WORST (highest-basis) leg prices the cap — a blend would let one
    /// expensive leg hide behind a cheap one.
    ///
    /// 跨账户双腿裁决（多账户解耦收口，docs/acceptance-account-decouple.md
    /// 披露 1）：pair 的经济单位 = (账户, 条件, 资产) —— 一个 UP share +
    /// 一个 DOWN share 在**同一账户的账本里**赎回 $1；每账户独立账本独立
    /// 赎回，跨账户的对手腿在结算时永不合并。所以分母只看本账户的对手腿：
    /// A 的 0.90 持仓不得把 B 的 0.05 新腿顶过 $1。
    pub(crate) fn counterpart_pair_basis(
        &self,
        condition_id: &str,
        asset: &str,
        direction: &SignalDirection,
        account_id: &AccountId,
    ) -> Option<Decimal> {
        self.positions
            .open_positions()
            .iter()
            .filter(|p| {
                p.condition_id == condition_id
                    && p.asset == asset
                    && p.direction != *direction
                    && p.shares > Decimal::ZERO
                    && &p.account_id == account_id
            })
            .map(|p| p.cost_usd / p.shares)
            .max()
    }

    pub(super) fn portfolio_limit_ok(
        &self,
        req: &crate::model::OrderRequest,
    ) -> Result<(), String> {
        // Read the LIVE risk config: a runtime update (risk_config_mut /
        // IPC) lands in the gate, not in the static CoreConfig.
        let cap = self.risk.config().max_open_notional_usd;
        if cap <= Decimal::ZERO {
            return Ok(());
        }
        // The cap is per-ACCOUNT: each account's notional pot is its own
        // (positions + resting buys of THAT account only) — A's full book
        // must not freeze B's fresh entry. The default scope's filter is a
        // no-op on a single-book deployment (every position already carries
        // `default`), so the historical numbers are unchanged.
        let used: Decimal = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == req.account_id)
            .map(|p| p.cost_usd)
            // #461: resting BUY orders are committed cash — they count
            // against the portfolio cap the moment they rest, not only
            // when they fill.
            .chain(
                self.ome
                    .live_orders()
                    .iter()
                    .filter(|o| o.side == Side::Buy && o.account_id == req.account_id)
                    .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO)),
            )
            .sum();
        let incoming = req.price * req.size;
        if used + incoming > cap {
            return Err(format!(
                "portfolio notional cap ({used}+{incoming} > {cap})"
            ));
        }
        Ok(())
    }

    /// Check a configured per-strategy entry cap against the strategy's current
    /// open exposure. Returns the rejection reason when a cap would be exceeded.
    pub(super) fn strategy_limit_ok(
        &self,
        req: &crate::model::OrderRequest,
        limit: &StrategyLimit,
        now_ms: i64,
    ) -> Result<(), String> {
        // The position-cap count judges LIVE positions only — an expired
        // round's stuck position is settlement-dead and must not consume
        // quota (the same rule PositionManager::can_open applies at the
        // global cap). The notional sum below keeps ALL positions: their
        // cost basis is cash still actually committed.
        let open: Vec<&crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == req.strategy && p.account_id == req.account_id)
            .collect();
        if let Some(max_pos) = limit.max_open_positions {
            let live = open.iter().filter(|p| p.expires_at_ms > now_ms).count();
            if live >= max_pos {
                return Err(format!(
                    "strategy {} at position cap ({live}/{max_pos})",
                    req.strategy
                ));
            }
        }
        if let Some(max_notional) = limit.max_open_notional_usd {
            let used: Decimal = open.iter().map(|p| p.cost_usd).sum::<Decimal>()
                // #461: the strategy's own resting BUY orders are committed
                // cash too — same rule as the portfolio cap. Both pots read
                // THIS account's slice (a per-strategy cap is per-scope).
                + self
                    .ome
                    .live_orders()
                    .iter()
                    .filter(|o| {
                        o.side == Side::Buy
                            && o.strategy == req.strategy
                            && o.account_id == req.account_id
                    })
                    .map(|o| (o.price * (o.size - o.filled_size)).max(Decimal::ZERO))
                    .sum::<Decimal>();
            let incoming = req.price * req.size;
            if used + incoming > max_notional {
                return Err(format!(
                    "strategy {} notional cap ({used}+{incoming} > {max_notional})",
                    req.strategy
                ));
            }
        }
        Ok(())
    }

    /// The strategy's virtual reservation outstanding on one account, for the
    /// readout / tests. Zero when the scope is not governed.
    pub fn virtual_used(&self, account_id: &str, strategy: &str) -> Decimal {
        self.virtual_accounts.used(account_id, strategy)
    }

    /// The configured virtual cap, when this scope has one.
    pub fn virtual_cap(&self, account_id: &str, strategy: &str) -> Option<Decimal> {
        self.virtual_accounts.cap(account_id, strategy)
    }

    /// RFC #541 挂钩 B — the WHOLE-CYCLE batch pre-check, the single point
    /// both call sites (the real cycle's submit pipeline and the nofast
    /// twin's `engine_replay_cycle`) share. For every declared-atomic
    /// strategy that emitted BUY legs this cycle, judge the strategy's legs
    /// AS ONE TICKET and remove every leg wholesale on a refusal:
    ///
    /// (a) credentials (Live only — a non-trading mode has no venue egress
    ///     and no credentials story, so replay/backtest corpora are not
    ///     batch-refused over shell env): any leg's venue lacking its
    ///     plugin's required credentials refuses the WHOLE strategy cycle.
    ///     Credentials are VENUE-scoped — judged across the strategy's full
    ///     cycle legs, NOT per `(account, strategy)` batch: the v2 dual-leg
    ///     stamp (entries and exits stamp the token's venue's leg account)
    ///     splits one pair across two accounts, so a per-account grouping
    ///     would judge each sub-batch against only its own legs and let the
    ///     credentialed venue's leg fly naked (RFC failure mode ①).
    /// (b) scope (all modes): any leg's token in conflict with its STAMPED
    ///     account's venue binding refuses the WHOLE strategy cycle — the
    ///     batch loop's last line of defence behind engine hook A (which
    ///     already drops declared-atomic strategies upstream, before any
    ///     OrderRequest exists).
    /// (c) the virtual pot (per governed account): each leg tries its
    ///     notional against ITS OWN (account, strategy) pot. A strategy may
    ///     legitimately span two accounts under the v2 stamp, so the pot is
    ///     the one dimension that stays per-account — an over-cap share
    ///     refuses the whole cycle so a partial pair never egresses.
    ///
    /// Undeclared strategies never enter (the grouping predicate is
    /// `strategy_atomic_batch`); unconfigured pots are transparent
    /// (`governed() == false` ⇒ that account's share always fits). Refusals
    /// bucket per leg under the strategy's (strategy, stamped account)
    /// accounting row with the SAME cause keys the batch loop carried
    /// (`credentials.notReady` / `scope.legRefused` / `limit.virtualCap`),
    /// bump `limit_rejected` per leg, log ONE `batch rejected:` INFO line
    /// naming the missing vars or the binding conflict, and strip every leg
    /// of the strategy from the cycle (0 legs to OME — no naked first leg).
    pub(super) fn arb_precheck_filter(
        &mut self,
        tokens: &mut Vec<(String, crate::model::OrderRequest)>,
    ) {
        let declared_atomic = |name: &str| {
            self.engine
                .as_ref()
                .is_some_and(|e| e.strategy_atomic_batch(name))
        };
        // Owned leg facts (the borrow split: the judges below need `&self`
        // while the rejection needs `&mut tokens`, so the cycle's leg list
        // is captured by value). Declared-atomic strategy → the cycle's
        // whole BUY leg list; a BTreeMap keeps multi-strategy traversal
        // deterministic.
        let mut cycles: std::collections::BTreeMap<String, Vec<CycleLeg>> =
            std::collections::BTreeMap::new();
        for (_, req) in tokens.iter() {
            if req.side == Side::Buy && declared_atomic(&req.strategy) {
                cycles
                    .entry(req.strategy.clone())
                    .or_default()
                    .push(CycleLeg {
                        internal_key: req.internal_key.clone(),
                        token: req.token_id.to_string(),
                        account: req.account_id.to_string(),
                        notional: req.price * req.size,
                    });
            }
        }
        for (strategy, legs) in cycles {
            // (a) credentials — venue-scoped, judged across the WHOLE cycle
            // once. The kernel's environment IS the credential truth (the
            // launcher loads `.env` before spawning this process; the old
            // config.json cell is retired), so the predicate reads `std::env`
            // directly — no per-cycle file load.
            if self.config.mode.may_trade() {
                let mut refusal: Option<String> = None;
                for leg in &legs {
                    let venue = self.execution_venue_of(&leg.account, &leg.token);
                    // tag 腿的 venue 插件必需凭证必须全部可解析（进程环境，
                    // 与 market::credentials_status 同一谓词；声明表来自
                    // 插件自声明，boot 聚合进 config）；
                    // untagged 腿不阻断——scope 门对无标签语料同样放行。
                    if !venue.is_empty()
                        && !venue_credentials_ready(&venue, &self.config.credential_vars)
                    {
                        // RFC #541 边界三（拒绝可见性）：the refusal names the
                        // leg, the venue, and the EXACT unresolved vars — the
                        // operator fixes the batch from this line alone.
                        let missing =
                            venue_credentials_missing(&venue, &self.config.credential_vars)
                                .join(",");
                        refusal = Some(format!(
                            "credentials.notReady: leg {} (token {}) venue `{venue}` has \
                             unresolved executor credentials: missing [{missing}]",
                            leg.internal_key, leg.token
                        ));
                        break;
                    }
                }
                if let Some(reason) = refusal {
                    self.reject_whole_cycle(
                        tokens,
                        &strategy,
                        &legs,
                        "credentials.notReady",
                        &reason,
                    );
                    continue;
                }
            }
            // (b) scope — the leg's STAMPED account's binding judges its own
            // leg (the v2 stamp precedes the gates intrinsically: 钉2). The
            // venue read is the leg's EXECUTION venue: on a shared token the
            // leg account's binding names its own (venue, token) row, so the
            // leg is judged against its OWN binding, never a sibling row's.
            let mut refusal: Option<String> = None;
            for leg in &legs {
                let venue = self.execution_venue_of(&leg.account, &leg.token);
                let bound = self
                    .accounts
                    .account(&AccountId::from(leg.account.as_str()))
                    .map(|a| a.venue.as_str())
                    .unwrap_or_default();
                if !bound.is_empty() && !venue.is_empty() && venue != bound {
                    refusal = Some(format!(
                        "scope.legRefused: leg {} (token {}) belongs to venue `{venue}` \
                         but account `{}` is bound to venue `{bound}`",
                        leg.internal_key, leg.token, leg.account
                    ));
                    break;
                }
            }
            if let Some(reason) = refusal {
                self.reject_whole_cycle(tokens, &strategy, &legs, "scope.legRefused", &reason);
                continue;
            }
            // (c) the virtual pot — per governed (account, strategy) scope:
            // the ONE dimension that stays per-account under the v2 stamp
            // (each leg's notional hits the pot of the account it spends).
            // An over-cap share refuses the whole cycle: a partial pair
            // never egresses, and the pot keeps its check-then-hold split
            // (the per-order `virtual_batch_commit` is still the ONLY
            // reservation point — DRY-RUN pre-checks hold nothing here).
            let mut over: Option<String> = None;
            for (account_id, share) in per_account_shares(&legs) {
                // DRY-RUN semantics (the pre-check must not hold anything —
                // the per-order loop's `virtual_batch_commit` is the ONLY
                // reservation point; reserving here too double-charged every
                // leg, the 4 red tests' ×2 signature): the occupied quota
                // OUTSIDE the pot is the strategy's open-position cost basis
                // on THIS account; resting BUY orders are NOT added (their
                // commitment IS their reservation — counting both would
                // double-charge the pot).
                let committed: Decimal = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.account_id.as_str() == account_id && p.strategy == strategy)
                    .map(|p| p.cost_usd)
                    .sum();
                if let Err(reason) = self
                    .virtual_accounts
                    .batch_fits(account_id, &strategy, &share, committed)
                {
                    over = Some(reason);
                    break;
                }
            }
            if let Some(reason) = over {
                self.reject_whole_cycle(tokens, &strategy, &legs, "limit.virtualCap", &reason);
                continue;
            }
        }
    }

    /// The wholesale side effect shared by all three dimensions: bucket every
    /// leg under its (strategy, stamped account) row, strip the strategy's
    /// legs from the cycle, log ONE auditable INFO line. `legs` is the
    /// strategy's full cycle leg list as captured BEFORE any removal.
    pub(super) fn reject_whole_cycle(
        &mut self,
        tokens: &mut Vec<(String, crate::model::OrderRequest)>,
        strategy: &str,
        legs: &[CycleLeg],
        cause: &str,
        reason: &str,
    ) {
        self.stats.strategy_limit_rejected += 1;
        for leg in legs {
            let acc = self
                .strategy_accounting
                .entry(strategy_acc_key(strategy, leg.account.as_str()))
                .or_default();
            acc.limit_rejected += 1;
            *acc.rejection_causes.entry(cause.to_string()).or_default() += 1;
        }
        tracing::info!(target: "strategy",
            "batch rejected: strategy={strategy} cause={cause} legs={} reason={reason}",
            legs.len());
        let refused: HashSet<&str> = legs.iter().map(|leg| leg.internal_key.as_str()).collect();
        tokens.retain(|(_, req)| !refused.contains(req.internal_key.as_str()));
    }

    /// The commit twin of the whole-cycle pre-check: it RUNS at the head of
    /// the per-order loop and holds the leg's quota until the state machine
    /// releases it. Called only when the pre-check passed, so the reserve
    /// cannot fail here — an unexpected refusal (a concurrent mutation
    /// cannot happen on `&mut self`) is logged loudly and degrades to the
    /// per-order path rather than wedging the cycle.
    pub(super) fn virtual_batch_commit(
        &mut self,
        account_id: &str,
        strategy: &str,
        batch: &[(String, Decimal)],
    ) {
        let _ = virtual_batch_commit_impl(self, account_id, strategy, batch);
    }

    /// E-VA full release of an order's remaining virtual reservation — the
    /// terminal-transition hook (cancelled / rejected / expired /
    /// force-closed). A no-op for SELL legs and ungoverned scopes. The
    /// fill-proportional conversion hook lives in `apply_delta_effects` (the
    /// one place a fill's signed delta and its LIMIT price are both at hand).
    pub(crate) fn virtual_release_order(&mut self, order: &crate::model::TrackedOrder) {
        if order.side != Side::Buy {
            return;
        }
        self.virtual_accounts.release_all(
            order.account_id.as_str(),
            &order.strategy,
            &order.internal_key,
        );
    }
}
