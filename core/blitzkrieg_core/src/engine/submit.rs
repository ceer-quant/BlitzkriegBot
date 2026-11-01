//! Split out of the parent module (issue #780): pure move.

use super::*;

impl Engine {
    /// The kernel-side half of one evaluation cycle: pending/dedup filters,
    /// the LIVE round-timing and momentum gates (near-miss and exemption
    /// trails included), sizing, and order construction. Shared verbatim by
    /// the real evaluation and the replay's quiet-tick cycles — one code
    /// path, so a replayed suggestion is judged exactly as a fresh one.
    pub(super) fn submit_candidate_orders(
        &mut self,
        candidates: Vec<TradeSignal>,
        now_ms: i64,
    ) -> Vec<crate::model::OrderRequest> {
        // E3: the table-level clock the per-candidate refinement falls back
        // to (a candidate whose market the scanner already dropped) — the
        // table's own family resolution, same shape as the callers read.
        let round = self.table_round_timing(now_ms);
        let mut orders = Vec::new();
        // At most one entry per token per cycle: the first candidate (in
        // registration order) wins, matching the one-live-entry-per-token rule
        // the caller enforces across cycles via `pending_tokens`.
        let mut emitted: HashSet<(String, String)> = HashSet::new();
        // ── RFC #541 方案 2a-i 挂钩 A（原型）：declared_atomic 策略的 scope
        // 判定先整组化。逐条路径里四个 scope 门（trading/frozen/exits_only/
        // admits_token）任一拒绝是「静默丢腿」——pair 的两条腿会在这里断开，
        // 第一腿照常进 OME（失败模式②）。对声明了 atomic_batch 的策略，先对
        // 本周期该策略的全部 candidates 做整组判定：任一条被 scope 门拒 ⇒
        // 该策略本周期全部 candidates 丢弃（0 腿到 OME），拒绝计数/审计保持
        // 逐条形状（见 atomic_scope_refusals 的用途），只加一条汇总 INFO。
        // 未声明 atomic_batch 的策略（spread_arb 等单所策略）不进这个分支，
        // 走下面的历史逐条路径，行为逐字节不变。pending_tokens 去重与
        // emitted 去重不是「拒绝」而是「已处理」，不参与整组判定。
        let mut atomic_scope_refusals: std::collections::BTreeMap<
            String,
            Vec<(&'static str, String)>,
        > = std::collections::BTreeMap::new();
        for sig in &candidates {
            if !self.strategy_atomic_batch(&sig.strategy) {
                continue;
            }
            // A candidate the scope gates refuse: record (gate, leg) — the
            // strategy's WHOLE cycle is then dropped below.
            let gate = if !self.scope_trading(&sig.account_id) {
                "scope_trading"
            } else if self.scope_frozen(&sig.account_id) {
                "scope_frozen"
            } else if self.scope_exits_only(&sig.strategy, &sig.account_id) {
                "scope_exits_only"
            } else if !self.scope_admits_token(&sig.account_id, &sig.token_id) {
                "scope_admits_token"
            } else {
                continue;
            };
            atomic_scope_refusals
                .entry(sig.strategy.clone())
                .or_default()
                .push((
                    gate,
                    format!(
                        "{}@{} (token {})",
                        sig.strategy, sig.account_id, sig.token_id
                    ),
                ));
        }
        // Any refused candidate ⇒ the strategy's WHOLE cycle is dropped:
        // zero legs out. Audited as one INFO line naming the gate(s) and the
        // refused legs (the per-strategy atomic batch is judged as a unit —
        // RFC #541's hook A).
        let dropped_atomic: HashSet<String> = atomic_scope_refusals.keys().cloned().collect();
        for (strategy, refusals) in &atomic_scope_refusals {
            let gates: Vec<&str> = refusals.iter().map(|(g, _)| *g).collect();
            tracing::info!(
                target: "strategy",
                "atomic batch dropped by scope filter: strategy={strategy} legs={} \
                 refused_legs={:?} gates={:?} — zero legs emitted this cycle",
                candidates.iter().filter(|s| &s.strategy == strategy).count(),
                refusals.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
                gates,
            );
        }
        // ── RFC 568 阶段一（补钉 5 的权威位）：A≠B 账户校验 —— 同一个自报
        // pair_key 下，两条腿盖出的 effective account_id 相同 ⇒ 拒绝该声明
        // （APPLICATION 错误，点名字段）。双腿落同账户的「配对」不是跨所
        // 套利（一笔单所双腿单 —— 配对簿的审批对象不存在），fail-closed 拒
        // 声明而非放行裸腿。只对带 pair 声明且未被 scope 门整组丢弃的腿判
        // 定；凭证不在这里校验（补钉 3 —— declare 在 arb_precheck 过闸之后）。
        // 校验通过 ⇒ 收进 pair_declarations，等 Core 在 precheck 过闸后 declare。
        {
            // 自报 pair_key → 该 key 下各腿的 effective account（本轮候选里）。
            let mut pair_accounts: std::collections::BTreeMap<&str, Vec<&str>> =
                std::collections::BTreeMap::new();
            for sig in &candidates {
                let Some(p) = &sig.pair else { continue };
                if dropped_atomic.contains(&sig.strategy) {
                    continue; // 整组已丢：无 effective 账户可言
                }
                pair_accounts
                    .entry(p.key.as_str())
                    .or_default()
                    .push(sig.account_id.as_str());
            }
            for sig in &candidates {
                let Some(p) = &sig.pair else { continue };
                if dropped_atomic.contains(&sig.strategy) {
                    continue;
                }
                let accounts = pair_accounts.get(p.key.as_str()).expect("seeded above");
                if accounts.len() >= 2 && accounts.iter().all(|a| *a == accounts[0]) {
                    let reason = format!(
                        "pair.declRefused: pair `{}` legs share effective account `{}` — \
                         both legs must stamp DIFFERENT accounts (a cross-venue pair, not a \
                         single-venue two-leg order); field: pair.key",
                        p.key, accounts[0]
                    );
                    tracing::info!(target: "strategy", "{reason}");
                    self.pair_refusals.push(reason);
                    continue;
                }
                self.pair_declarations.push(PairDeclaration {
                    owner_strategy: sig.strategy.clone(),
                    // 实例主账户：该策略实例自己的 scope（注册时的
                    // account_id，trait 访问器 `instance_account`），不是腿
                    // 的盖章结果 —— 实例隔离维度（跨实例同名 pair_key 不串
                    // 账）；跨同名多实例的歧义按第一实例落（与下游以
                    // (strategy, 腿账户) 寻址的判定链同口径）。
                    owner_account: self
                        .strategies
                        .iter()
                        .find(|s| s.strategy.name() == sig.strategy)
                        .map(|s| s.strategy.instance_account())
                        .unwrap_or_else(|| sig.account_id.clone()),
                    leg_account: sig.account_id.clone(),
                    decl: crate::signal::TradePairDecl {
                        key: p.key.clone(),
                        leg: p.leg.clone(),
                    },
                    token_id: sig.token_id.clone(),
                    shares: sig.shares,
                });
            }
        }
        for sig in candidates {
            if dropped_atomic.contains(&sig.strategy) {
                continue;
            }
            // Account-book facts FIRST (before any gate can record a near-miss
            // or burn a cooldown): a scope whose trading switch is OFF
            // contributes nothing, and a scope bound to venue V never prices
            // another venue's token. Exits bypass this entire path.
            if !self.scope_trading(&sig.account_id) {
                continue;
            }
            // Account-scoped kill switch (`account.freeze`): a frozen scope is
            // invisible to entries, exactly like a trading-OFF one. Exits
            // bypass this whole path, so held positions stay managed.
            if self.scope_frozen(&sig.account_id) {
                continue;
            }
            // The exits-only demotion (`account.strategies.set` safety net):
            // a removed-but-held scope keeps its exits flowing but emits no
            // NEW entries, per (strategy, account) — the same strategy on a
            // still-assigned account keeps trading normally.
            if self.scope_exits_only(&sig.strategy, &sig.account_id) {
                continue;
            }
            if !self.scope_admits_token(&sig.account_id, &sig.token_id) {
                continue;
            }
            if self
                .pending_tokens
                .contains(&(sig.account_id.clone(), sig.token_id.clone()))
            {
                continue;
            }
            // One entry per (scope, token) per cycle: account A's candidate
            // for a token never blocks account B's.
            if !emitted.insert((sig.account_id.clone(), sig.token_id.clone())) {
                continue;
            }
            // Multi-plugin data plane: the timing verdict is PER-CANDIDATE —
            // the expiry gate reads the candidate's own venue clock, so a
            // polymarket market inside its force-exit window no longer freezes
            // a kalshi candidate in the same evaluation (and vice versa).
            // Presence/age stay the global verdict; the per-market form only
            // refines which expiry applies. Without a market (stale signal for
            // a token the scanner dropped) the whole-table verdict decides.
            // E3: the same per-market refinement now carries the FAMILY — the
            // candidate's own declared duration grid (its own expiry clock
            // and its slot), so the whole gate trail below (D-31 floor,
            // near-miss telemetry, block/exemption stamps, the internal
            // key's round segment) reads numbers from the candidate's clock,
            // not the global grid. Single-family tables resolve every row to
            // the configured grid, byte-identical with the pre-E3 read.
            let cand_round = match self.scanner.market_for_token(&sig.token_id) {
                Some(m) => self.family_round_timing(m, now_ms),
                None => round,
            };
            let timing = match self.scanner.market_for_token(&sig.token_id) {
                Some(m) => self.scanner.can_trade_reason_for(m, now_ms).err(),
                None => self.scanner.can_trade_reason(now_ms).err(),
            };
            if let Some(block) = timing {
                // E2-b: a strategy may declare the timing WINDOW gates unnecessary
                // for its own candidates. The structural precondition (no market
                // this round) is not waivable, and everything downstream in
                // `Core` (risk gate, kill switch, daily loss, quotas, sizing) is
                // untouched by this path.
                //
                // D-31: the exemption is honoured only while `time_left_sec` is at
                // or above the strategy's declared floor (default: the scanner's
                // own `min_time_left_sec`). Before this, an exempt strategy could
                // enter inside the force-exit window, where the exit policy fires
                // on the very next tick — a correctly-priced but guaranteed
                // zero-hold exit that only put noise into the ledger. A "too young"
                // round has a large `time_left_sec`, so it stays waived; the block
                // this actually stops is "too close to expiry", which by definition
                // sits below the floor.
                let waived = block.exemptible()
                    && self
                        .strategy_gate_exemptions(&sig.strategy)
                        .is_some_and(|e| {
                            e.timing
                                && cand_round.time_left_sec
                                    >= e.timing_floor_sec(self.cfg.scanner.min_time_left_sec)
                        });
                if !waived {
                    let mid = self
                        .book_snapshot(&sig.token_id)
                        .map(|b| b.mid_price)
                        .unwrap_or(Decimal::ZERO);
                    self.near_miss.on_blocked(
                        &sig.token_id,
                        &sig.asset,
                        sig.direction.as_str(),
                        sig.price,
                        mid,
                        crate::shadow::NearMissReason::Timing,
                        cand_round.time_left_sec,
                        cand_round.slot,
                        now_ms,
                    );
                    self.last_blocked.push(BlockedCandidate {
                        strategy: sig.strategy,
                        account_id: sig.account_id,
                        token_id: sig.token_id,
                        asset: sig.asset,
                        direction: sig.direction,
                        price: sig.price,
                        reason: BlockReason::Timing,
                        time_left_sec: cand_round.time_left_sec,
                    });
                    continue;
                }
                self.last_exemptions.push(GateExemptionRecord {
                    strategy: sig.strategy.clone(),
                    account_id: sig.account_id.clone(),
                    gate: "timing",
                    token_id: sig.token_id.clone(),
                    asset: sig.asset.clone(),
                    detail: block.to_string(),
                    time_left_sec: cand_round.time_left_sec,
                });
            }
            if let Err(detail) = self.momentum_ok(&sig.asset, sig.direction, now_ms) {
                let waived = self
                    .strategy_gate_exemptions(&sig.strategy)
                    .is_some_and(|e| e.momentum);
                if !waived {
                    let mid = self
                        .book_snapshot(&sig.token_id)
                        .map(|b| b.mid_price)
                        .unwrap_or(Decimal::ZERO);
                    self.near_miss.on_blocked(
                        &sig.token_id,
                        &sig.asset,
                        sig.direction.as_str(),
                        sig.price,
                        mid,
                        crate::shadow::NearMissReason::Momentum,
                        cand_round.time_left_sec,
                        cand_round.slot,
                        now_ms,
                    );
                    self.last_blocked.push(BlockedCandidate {
                        strategy: sig.strategy,
                        account_id: sig.account_id,
                        token_id: sig.token_id,
                        asset: sig.asset,
                        direction: sig.direction,
                        price: sig.price,
                        reason: BlockReason::Momentum,
                        time_left_sec: cand_round.time_left_sec,
                    });
                    continue;
                }
                self.last_exemptions.push(GateExemptionRecord {
                    strategy: sig.strategy.clone(),
                    account_id: sig.account_id.clone(),
                    gate: "momentum",
                    token_id: sig.token_id.clone(),
                    asset: sig.asset.clone(),
                    detail,
                    time_left_sec: cand_round.time_left_sec,
                });
            }

            // A strategy-declared share count (pair legs must match in SHARES,
            // not in notional) is honoured only inside the kernel's risk band:
            // capped at the same `max_shares` ceiling the notional path obeys,
            // so a declaration can never oversize. When the equity-relative
            // mode is armed (#202), the declared count is ALSO capped by the
            // strategy's whole-share equity ticket — a percentage budget is an
            // upper bound on committed cash, and a depth-based declaration is
            // exactly the path that would otherwise spend past it. The cap is
            // uniform across a signal batch, so a pair's two legs (declared
            // equal, clamped by the same budget at near-equal prices) stay
            // mergeable. A declared count below one share buys NOTHING —
            // falling back to notional sizing would open the naked leg of a
            // pair — and `None` keeps notional sizing.
            let size = match sig.shares {
                Some(n) => {
                    if n < Decimal::ONE {
                        continue;
                    }
                    let sizing = self.effective_sizing(&sig.strategy);
                    let mut capped = n.min(sizing.max_shares);
                    if sizing.size_pct > Decimal::ZERO {
                        let ticket = self.equity_shares(sig.price, &sizing, &sig.account_id);
                        if ticket <= Decimal::ZERO {
                            self.size_pct_skipped += 1;
                            continue;
                        }
                        capped = capped.min(ticket);
                    }
                    capped
                }
                // #202: an equity-relative budget too small for one whole share
                // produces NO order (counted, not emitted as a 0-size rejection).
                None => {
                    let Some(size) = self.entry_ticket(sig.price, &sig.strategy, &sig.account_id)
                    else {
                        self.size_pct_skipped += 1;
                        continue;
                    };
                    size
                }
            };
            orders.push(crate::model::OrderRequest {
                token_id: sig.token_id.clone(),
                condition_id: sig.condition_id,
                side: crate::model::Side::Buy,
                // `entry_maker` doctrine (manifest `entry_maker` / trait
                // `entry_maker`): a pure-maker package's legs rest post-only
                // at their quoted price and NEVER escalate — the submission
                // sites answer with `maker_timeout_ms = -1` (the OME's
                // never-escalate clock). Every other strategy keeps the
                // `MakerThenTaker` entry (maker first, taker chase at
                // `entry_maker_timeout_ms`), byte-identically.
                mode: if self.strategy_entry_maker(&sig.strategy) {
                    crate::model::FillPolicy::Maker
                } else {
                    crate::model::FillPolicy::MakerThenTaker
                },
                price: sig.price,
                size,
                internal_key: if sig.account_id == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
                    // Default scope: byte-identical to the pre-fanout key —
                    // the replay baseline compares these strings. E3: the
                    // slot segment is the CANDIDATE's own family slot (the
                    // single-family corpora produce the same numbers).
                    format!(
                        "{}:{}:{}:{}",
                        sig.strategy,
                        sig.asset,
                        sig.direction.as_str(),
                        cand_round.slot
                    )
                } else {
                    // A second scope on the same strategy/asset/slot must not
                    // collide with the default scope's key in the OME's
                    // global dedup: the account segment disambiguates.
                    format!(
                        "{}:{}:{}:{}:{}",
                        sig.strategy,
                        sig.asset,
                        sig.direction.as_str(),
                        cand_round.slot,
                        sig.account_id
                    )
                },
                strategy: sig.strategy,
                asset: sig.asset.clone(),
                direction: sig.direction.as_str().to_string(),
                round_slot: cand_round.slot,
                // E28 §9.2, extended by the multi-account fanout: the signal
                // carries its SCOPE's account (the default scope keeps the
                // process default, byte-identical to the old behaviour).
                account_id: blitzkrieg_market_api::AccountId(sig.account_id.clone()),
            });
        }
        orders
    }

    /// Drain finalized near-miss records for persistence.
    pub fn take_near_misses(&mut self, now_ms: i64) -> Vec<crate::shadow::NearMissRecord> {
        self.near_miss.tick(now_ms)
    }

    /// #427: the venue the token's market listed on, read off the current
    /// round's market list — pure DATA for the panels' Venue column (a
    /// cross-venue pair shows one leg per venue). `None` when the engine is
    /// down or the token belongs to no live market; the caller renders that
    /// as the undeclared case.
    ///
    /// RFC-661 M1: token-only resolution admits ONLY a unique candidate —
    /// ambiguity (≥2 venues hold rows for the token) refuses with `None`,
    /// never first-hit by insertion order (the old "first venue to arrive
    /// owns the token" the mirror shape depended on).
    pub fn venue_of(&self, token_id: &str) -> Option<String> {
        let venues = self.scanner.venues_holding_token(token_id);
        if venues.len() != 1 {
            return None;
        }
        self.scanner
            .market_for_token_on_venue(&venues[0], token_id)
            .map(|m| m.venue.clone())
            .filter(|v| !v.is_empty())
    }

    /// The venue that should EXECUTE `token` for `account`. RFC-661 M1: the
    /// table is (venue, token) dual-keyed — each venue holds its OWN row for
    /// a byte-identical token — so the row's venue IS the execution venue,
    /// insertion order irrelevant (the data lands already attributed; RFC-661
    /// M3 retired the mirror-override arm with the whole bridge). A binding
    /// whose venue holds the token's row executes on ITS OWN venue. No own
    /// row: the token-only lookup admits ONLY a unique candidate — a single
    /// row carrying the token — and an untagged row (the legacy bucket)
    /// routes to the binding. Ambiguity (≥2 venues hold the token, none of
    /// them the binding's) refuses (None), never first-hit by insertion
    /// order. Unbound accounts ride the same unique-candidate lookup,
    /// byte-identical with the single-venue routing. The executor drain's
    /// routing truth: per-venue drains key on this, so a shared token's two
    /// accounts each reach their OWN executor and the poly account's live
    /// order can never land on the real venue (nor the reverse). The account
    /// parameter is the AMBIGUITY DISCRIMINATOR — on a shared token a
    /// token-only read refuses (`venue_of` → None), so it stays (the M4 map
    /// conjectured it was redundant; the pinned drain contract says
    /// otherwise).
    pub fn execution_venue_for(&self, account: &str, token_id: &str) -> Option<String> {
        let bound = self.venue_by_account.get(account).filter(|v| !v.is_empty());
        if let Some(b) = bound {
            if let Some(m) = self.scanner.market_for_token_on_venue(b, token_id) {
                // The binding's own dual-key row: its venue IS the executor
                // (untagged rows name no venue — fall through to the
                // unique-candidate read, which resolves them the old way).
                if !m.venue.is_empty() {
                    return Some(b.clone());
                }
            } else if let Some(v) = self.venue_of(token_id) {
                // Unique candidate on the table, none of it the binding's:
                // the row's venue is the executor (an untagged unique row
                // keeps the legacy "routes to the binding" reading).
                if v.is_empty() {
                    return Some(b.clone());
                }
                return Some(v);
            }
            return None;
        }
        self.venue_of(token_id)
    }

    /// The condition's OTHER outcome token, read off the current round's
    /// market list — the dry settlement ladder's counter-evidence key. The
    /// binary market structure guarantees a held token's partner exists while
    /// the market is live; `None` when the token belongs to no live market
    /// (the caller stamps empty and the ladder keeps its withhold behavior).
    ///
    /// RFC-661 M1: token-only resolution admits ONLY a unique candidate —
    /// ambiguity (≥2 venues hold rows for the token) refuses with `None`,
    /// never first-hit by insertion order. Callers that know their venue read
    /// [`Self::market_for_token_on_venue`]. A partner token IS itself shared
    /// by both rows (the sibling instrument), which is fine: the ladder's
    /// counter-evidence key names the condition's other side, not a venue.
    pub fn partner_of(&self, token_id: &str) -> Option<String> {
        self.scanner.market_for_token(token_id).map(|m| {
            if m.up_token_id == token_id {
                m.down_token_id.clone()
            } else {
                m.up_token_id.clone()
            }
        })
    }

    /// #461: whether the token's market settles through the NegRisk adapter —
    /// the merge call's routing key. `None`/`false` merge through the plain
    /// CTF; a neg-risk market would revert there, so the request must say so.
    /// RFC-661 M1: unique-candidate resolution (ambiguity refuses, never
    /// first-hit) — see [`Self::market_for_token`].
    pub fn neg_risk_of(&self, token_id: &str) -> Option<bool> {
        self.scanner.market_for_token(token_id).map(|m| m.neg_risk)
    }

    /// The token's live round slot (the market that currently lists it).
    /// The pair-chase request's round identity: a NEXT round's orphan gets a
    /// fresh internal key, so a retried chase never collides with the OME's
    /// duplicate-intent guard. `None` = the token belongs to no live market.
    /// RFC-661 M1: unique-candidate resolution (ambiguity refuses, never
    /// first-hit) — see [`Self::market_for_token`].
    pub fn round_slot_of(&self, token_id: &str) -> Option<i64> {
        self.scanner
            .market_for_token(token_id)
            .map(|m| m.round_slot)
    }

    /// The token's market asset (the request's provenance echo).
    /// RFC-661 M1: unique-candidate resolution (ambiguity refuses, never
    /// first-hit) — see [`Self::market_for_token`].
    pub fn asset_of(&self, token_id: &str) -> Option<String> {
        self.scanner
            .market_for_token(token_id)
            .map(|m| m.asset.clone())
    }

    /// The token's leg label (`"up"` / `"down"`, read off its market side —
    /// the same wire direction the engine stamps on entry requests).
    /// RFC-661 M1: unique-candidate resolution (ambiguity refuses, never
    /// first-hit) — see [`Self::market_for_token`].
    pub fn direction_of(&self, token_id: &str) -> Option<SignalDirection> {
        self.scanner.market_for_token(token_id).map(|m| {
            if m.up_token_id == token_id {
                SignalDirection::Up
            } else {
                SignalDirection::Down
            }
        })
    }

    /// Force-finalize every pending near-miss (used on shutdown so records are
    /// not lost when the process stops before a window elapses).
    pub fn flush_near_misses(&mut self) -> Vec<crate::shadow::NearMissRecord> {
        self.near_miss.flush()
    }

    /// Near-miss signals blocked on the most recent evaluation.
    pub fn last_blocked(&self) -> &[BlockedCandidate] {
        &self.last_blocked
    }
    pub fn blocked_timing_count(&self) -> u64 {
        self.blocked_timing_total
    }
    pub fn blocked_momentum_count(&self) -> u64 {
        self.blocked_momentum_total
    }

    /// The per-strategy gate exemptions registered strategies declared
    /// (E2-b / #27). None for an unknown strategy.
    /// E25 (#331): the token ids tradable THIS round (both sides of every
    /// market) — Gate 1's "token 属于本轮" check reads this.
    pub fn round_token_ids(&self) -> Vec<String> {
        self.scanner
            .markets()
            .iter()
            .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
            .collect()
    }

    /// E25 (#331): `time_left_sec` of the most recent evaluate cycle.
    pub fn last_time_left_sec(&self) -> i64 {
        self.last_time_left_sec
    }

    pub fn strategy_gate_exemptions(&self, name: &str) -> Option<GateExemptions> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.strategy.gate_exemptions())
    }

    /// Replay fast path: the earliest virtual instant the round-timing
    /// verdict (`scanner.can_trade_reason`) can flip for ANY registered
    /// strategy, computed from the round clock's integer-second fields —
    /// the min-round-age start and the min-time-left floor, each pulled
    /// earlier by 1 s so the wake tick lands at or before the boundary a
    /// full evaluation would have seen. Strategies with a timing exemption
    /// contribute their own (lower) floor. `None` when no market is loaded
    /// (the next rollover re-arms the clock via `fast_next_timing_ms = 0`).
    ///
    /// A tick that evaluates before this instant provably reproduces the
    /// previous evaluation: the round clock's second-hand fields are the
    /// only strategy inputs that move with `now_ms` between events, and
    /// their comparisons cannot change verdict inside the open interval.
    pub fn next_timing_boundary_ms(&self, now_ms: i64) -> Option<i64> {
        if self.scanner.markets().is_empty() {
            return None;
        }
        // E3: the table's own family clock (single-family tables resolve the
        // duration to the configured grid — the same numbers the global read
        // produced). NOT one of the audited strategy-visible sites: this is
        // the replay fast path's WAKE predictor, and its conservative
        // whole-table expiry stays the correct wake bound for every family.
        let round = self.table_round_timing(now_ms);
        let expires = round.expires_at_ms;
        let t = round.time_left_sec.max(0);
        let mut best: i64 = i64::MAX;
        // A timing verdict flips when time_left_sec first reads a boundary
        // value. time_left reads V-1 first at `expires - V*1000 + 1` and
        // reads V first at `expires - V*1000 - 999` (integer-second windows).
        // A boundary at/behind now clamps to now+1: the next tick re-checks
        // (cheap, and self-correcting if the arithmetic rounds early).
        let consider = |b: i64, best: &mut i64| {
            if b <= now_ms {
                return;
            }
            if b < *best {
                *best = b;
            }
        };
        // Per-strategy time-left floor: allowed while time_left >= floor,
        // flips when it first reads floor-1.
        for s in &self.strategies {
            let e = s.strategy.gate_exemptions();
            let floor = if e.timing {
                e.timing_floor_sec(self.cfg.scanner.min_time_left_sec)
            } else {
                self.cfg.scanner.min_time_left_sec
            };
            if floor > 0 && t >= floor {
                consider(expires - floor * 1000 + 1, &mut best);
            }
        }
        // Min-round-age: blocked while age < min_age; flips when time_left
        // first reads (duration - min_age). E3: the duration is the table's
        // resolved family's (single-family = the configured grid, verbatim).
        let min_age = self.cfg.scanner.min_round_age_sec.max(0);
        if round.age_sec < min_age {
            let v = round.age_sec - min_age + round.time_left_sec;
            if v > 0 {
                consider(expires - v * 1000 - 999, &mut best);
            }
        }
        if best == i64::MAX {
            None
        } else {
            Some(best.max(now_ms + 1))
        }
    }

    /// Every declared exemption, as `(strategy, exemptions)` in registration
    /// order — the audit view of who opted out of what.
    pub fn declared_gate_exemptions(&self) -> Vec<(String, GateExemptions)> {
        self.strategies
            .iter()
            .map(|s| (s.strategy.name().to_string(), s.strategy.gate_exemptions()))
            .filter(|(_, e)| e.any())
            .collect()
    }

    /// Whether the named strategy declared hold-to-settlement semantics: its
    /// positions are meant to be redeemed at expiry, not sold on the exit
    /// ladder (a complete-set pair pays $1 at settlement regardless of which
    /// side wins — selling a leg before expiry would destroy that payoff).
    /// Read by the host's exit checks; `false` for an unknown strategy.
    pub fn strategy_holds_to_settlement(&self, name: &str) -> bool {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.holds_to_settlement())
    }

    /// E-VA: whether the named strategy declared ATOMIC-BATCH semantics — its
    /// candidates in one evaluation cycle reserve the virtual quota as ONE
    /// batch before the first leg goes out. `false` for an unknown strategy
    /// and for every strategy that did not declare it (the per-order path).
    pub fn strategy_atomic_batch(&self, name: &str) -> bool {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.atomic_batch())
    }

    /// The entry fill-policy doctrine (`entry_maker`) of the named strategy:
    /// `true` = the host submits this strategy's entries post-only with the
    /// never-escalate clock. Unknown strategy → false (the default trait
    /// verdict; fail-closed to today's `MakerThenTaker`).
    pub fn strategy_entry_maker(&self, name: &str) -> bool {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.entry_maker())
    }

    /// RFC-645 A-1 (决议③): whether the named strategy's DECLARATION to
    /// consume the pair-book statistics surface is ACCEPTED. The predicate is
    /// deliberately three-legged and fail-closed:
    ///
    /// 1. the strategy declared `consumes_pair_book_stats()` (the trait's
    ///    declaration face), AND
    /// 2. it declared `atomic_batch()` (a pair consumer that does not run
    ///    atomic batches cannot keep its legs whole), AND
    /// 3. the PairBook holds at least one live (non-Closed) entry whose
    ///    `owner_strategy` is this strategy (it actually produces pairs).
    ///
    /// Any unmet leg = NOT accepted (the row carries no `arb*` fields) —
    /// the host-side warn lives in `Core::strategy_stats`, which owns the
    /// reasons per strategy (one warn per stats read, per strategy, when a
    /// declaration is present but not accepted).
    pub(crate) fn pair_book_stats_accepted(
        &self,
        name: &str,
        pair_book: &crate::pair::PairBook,
    ) -> bool {
        let declared = self
            .strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.consumes_pair_book_stats());
        declared && self.strategy_atomic_batch(name) && pair_book.active_pairs_count(name) > 0
    }

    /// The RFC-645 A-1 acceptance diagnosis for ONE strategy name: which of
    /// the three predicate legs is unmet, as a human-readable clause for the
    /// host's warn. `None` = either the declaration is absent entirely (the
    /// strategy never claimed the surface — silent default) or ALL three legs
    /// hold (the strategy IS accepted — nothing to diagnose).
    pub(crate) fn pair_book_stats_rejection_reason(
        &self,
        name: &str,
        pair_book: &crate::pair::PairBook,
    ) -> Option<&'static str> {
        let declared = self
            .strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.consumes_pair_book_stats());
        if !declared {
            return None;
        }
        if !self.strategy_atomic_batch(name) {
            return Some("atomic_batch not declared");
        }
        if pair_book.active_pairs_count(name) == 0 {
            return Some("no live PairBook entry under this strategy");
        }
        None
    }

    /// True when EVERY ENABLED strategy holds to settlement — the replay
    /// fast path's precondition for skipping the exit ladder between dirty
    /// ticks (a ladder verdict against a holder is discarded by the host
    /// anyway, so re-running it per maintenance tick with a frozen book is
    /// pure waste). False when any enabled non-holder is registered or the
    /// engine is absent, so mixed books keep the full ladder.
    ///
    /// Disabled strategies are excluded deliberately: a disabled strategy
    /// contributes nothing (no evaluation, no orders, no exits — its hook
    /// output is dropped, see `drain_breaks`), so during a replay it can
    /// never hold a position and its non-holder declaration must not veto
    /// the fast path. A registry of nine libraries with one enabled holder
    /// is exactly the deployment shape the fast path is for.
    pub fn all_strategies_hold_to_settlement(&self) -> bool {
        self.strategies
            .iter()
            .filter(|s| s.enabled)
            .all(|s| s.strategy.holds_to_settlement())
    }

    /// Gate exemptions honoured on the most recent evaluation, in the order the
    /// candidates were evaluated. Cleared at the start of every `evaluate`.
    pub fn last_exemptions(&self) -> &[GateExemptionRecord] {
        &self.last_exemptions
    }

    /// Drain the honoured-exemption records (called by the host after evaluate)
    /// and fold them into the per-strategy tallies (per-name AND per-account).
    pub fn take_exemptions(&mut self) -> Vec<GateExemptionRecord> {
        let out = std::mem::take(&mut self.last_exemptions);
        for r in &out {
            let t = self.gate_tally.entry(r.strategy.clone()).or_default();
            match r.gate {
                "timing" => t.exempted_timing += 1,
                _ => t.exempted_momentum += 1,
            }
            let key = format!("{}\u{1}{}", r.strategy, r.account_id);
            let t = self.gate_tally_acc.entry(key).or_default();
            match r.gate {
                "timing" => t.exempted_timing += 1,
                _ => t.exempted_momentum += 1,
            }
        }
        out
    }

    /// Blocked / exempted counts for one strategy (E2-b / #27).
    pub fn gate_tally(&self, name: &str) -> StrategyGateTally {
        self.gate_tally.get(name).copied().unwrap_or_default()
    }

    /// Blocked / exempted counts for one (strategy, account) scope — the
    /// account slice the per-account strategy page reads. Zeros when the
    /// scope never saw a gate verdict.
    pub fn gate_tally_for_account(&self, name: &str, account: &str) -> StrategyGateTally {
        let key = format!("{}\u{1}{}", name, account);
        self.gate_tally_acc.get(&key).copied().unwrap_or_default()
    }

    /// Accumulate blocked counters (called by the caller after evaluate).
    pub fn tally_blocked(&mut self) {
        for b in &self.last_blocked {
            match b.reason {
                BlockReason::Timing => self.blocked_timing_total += 1,
                BlockReason::Momentum => self.blocked_momentum_total += 1,
            }
            let t = self.gate_tally.entry(b.strategy.clone()).or_default();
            match b.reason {
                BlockReason::Timing => t.blocked_timing += 1,
                BlockReason::Momentum => t.blocked_momentum += 1,
            }
            let key = format!("{}\u{1}{}", b.strategy, b.account_id);
            let t = self.gate_tally_acc.entry(key).or_default();
            match b.reason {
                BlockReason::Timing => t.blocked_timing += 1,
                BlockReason::Momentum => t.blocked_momentum += 1,
            }
        }
    }

    /// Attach (or detach) the Shadow Evolution per-strategy parameter registry.
    /// Called when evolution is (re)configured; idempotent. Forwarded to every
    /// registered strategy, which resolves its OWN cell from it (E2-c / #28).
    ///
    /// `None` DETACHES: strategies fall back to the config pushed via
    /// `on_config`, which is what makes "evolution disabled" provably identical
    /// to the pre-feature behaviour rather than merely inert by convention.
    pub fn set_hot_params(
        &mut self,
        registry: Option<std::sync::Arc<crate::registry::ParamRegistry>>,
    ) {
        self.hot_params = registry.clone();
        for s in &mut self.strategies {
            s.strategy.set_hot_params(registry.clone());
        }
    }

    pub fn has_hot_params(&self) -> bool {
        self.hot_params.is_some()
    }

    /// Per-strategy config-in-force views as (name, JSON string) for every
    /// strategy that reports one (observability). An external library reports
    /// through its OPTIONAL `bk_strategy_config_view` symbol; strategies that
    /// declare nothing are simply absent. The kernel keeps no knowledge of any
    /// strategy's parameter names.
    pub fn strategy_config_views(&self) -> Vec<(String, String)> {
        self.strategies
            .iter()
            .filter_map(|s| {
                s.strategy
                    .config_view_json()
                    .map(|v| (s.strategy.name().to_string(), v))
            })
            .collect()
    }

    /// Strategy names the engine knows about, in registration order (PR-B: the
    /// kernel ships none, so this is exactly what the loader registered).
    pub fn supported_strategies(&self) -> Vec<String> {
        self.strategies
            .iter()
            .map(|s| s.strategy.name().to_string())
            .collect()
    }
    /// Borrow every hosted strategy. Shadow Evolution asks each one which knobs it
    /// declares (E2-c), so the declaration has to be read off the live instances.
    ///
    /// Deliberately NOT filtered by `enabled`: `register_strategies` drops the cell
    /// of any strategy it is not handed, so filtering here would make a runtime
    /// disable destroy that strategy's evolved parameters and rollback anchor.
    /// A switched-off strategy just never emits the candidates that would use them.
    pub fn strategy_refs(&self) -> Vec<&dyn EngineStrategy> {
        self.strategies
            .iter()
            .map(|s| s.strategy.as_ref())
            .collect()
    }
    /// Enabled strategy names.
    pub fn enabled_strategies(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .strategies
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.strategy.name().to_string())
            .collect();
        v.sort();
        v
    }

    /// RFC 568 阶段一：borrow every hosted scope (name/enabled/account per
    /// instance) —— Core 侧构建 per-owner 的配对簿视图快照用（views_for 的
    /// owner 过滤要实例主账户 + arb 腿账户白名单，两者都在 HostedStrategy
    /// 上）。crate 内可见：host 内部接线，非公开 API 面。
    pub(crate) fn hosted(&self) -> &[HostedStrategy] {
        &self.strategies
    }
    /// Toggle a strategy; returns false if the name is unknown to the engine.
    ///
    /// Multi-scope semantics (账户↔venue 解耦裁决): the toggle is the
    /// GLOBAL strategy switch — the master power for every scope instance of
    /// `name`, default scopes included (a global OFF stops the strategy
    /// everywhere; a global ON hands the decision back to each account's
    /// enablement set). Per-account selectivity is the account book's
    /// `strategies` ENABLEMENT set (which per-account scopes run), not this
    /// switch: an account that has the strategy in its set keeps running it
    /// while the global switch is ON, regardless of other accounts' sets.
    ///
    /// PERSISTENCE ASYMMETRY (issue 552 audit, honest wording): a global OFF
    /// is a session-level fact on the default scope's `enabled` flag — it is
    /// NOT persisted anywhere, so a kernel restart hands the decision back to
    /// the persisted enablement sets again (a fresh boot replays
    /// accounts.toml, and a scope named in an account's set boots ENABLED —
    /// see the scope-rebuild rule). A global ON is likewise session-level.
    /// The DURABLE per-account truth is only the enablement set: to keep a
    /// strategy off across restarts, remove it from every account's set
    /// (the panel's per-account 启用按钮 / `account.strategies.set`).
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let mut found = false;
        for s in &mut self.strategies {
            if s.strategy.name() == name {
                s.enabled = enabled;
                found = true;
            }
        }
        found
    }

    /// Register a strategy into the live dispatch.
    ///
    /// This is the ONLY way a strategy enters the engine. The kernel ships none
    /// (PR-B): external v2 dylibs come through the C ABI v2 loader, which calls
    /// this. It starts DISABLED: an explicit enable is required before it can
    /// place orders. Returns the registered name, or an error when the name is
    /// taken.
    pub fn register_user_strategy(
        &mut self,
        strategy: Box<dyn EngineStrategy>,
        source: String,
    ) -> Result<String, String> {
        self.register_scoped_strategy(strategy, source, blitzkrieg_market_api::DEFAULT_ACCOUNT_ID)
    }

    /// Register a strategy instance into ONE account scope. The (account,
    /// name) pair must be unique — two scopes may both host `spread_arb`,
    /// each with its own Lua state, sizing and book; one scope cannot host
    /// the same strategy twice. Starts DISABLED like the default-scope form.
    pub fn register_scoped_strategy(
        &mut self,
        strategy: Box<dyn EngineStrategy>,
        source: String,
        account_id: &str,
    ) -> Result<String, String> {
        let name = strategy.name().to_string();
        if self
            .strategies
            .iter()
            .any(|s| s.strategy.name() == name && s.account_id == account_id)
        {
            return Err(format!(
                "strategy name already registered for account {account_id}: {name}"
            ));
        }
        // Provenance is sticky: the FIRST registration's source wins, so a
        // scope teardown can never orphan a re-assignment.
        self.package_sources
            .entry(name.clone())
            .or_insert_with(|| source.clone());
        let mut hosted = HostedStrategy {
            strategy,
            enabled: false,
            source,
            account_id: account_id.to_string(),
            exits_only: false,
        };
        // Hand the host config to the newly registered strategy — the SAME
        // JSON projection every other push site uses (RFC-645 A-2, 决议④:
        // one marshal, one wire shape, set_config dispatch and birth feed
        // alike; the dispatch TIMING here is unchanged — still synchronous at
        // registration, before the strategy can be enabled).
        let birth_config = engine_strategy_config_json(&self.cfg.trend, &self.cfg.spread_arb);
        hosted.strategy.on_config(&birth_config);
        // Registered after Shadow Evolution was configured? Forward the handle
        // so a later evolution still reaches this strategy.
        if let Some(h) = &self.hot_params {
            hosted.strategy.set_hot_params(Some(h.clone()));
        }
        // 跨所套利弹窗 v2: a scope registered while an arb configuration is
        // already in force receives ITS PROJECTION at birth (the same
        // "later joiner" pattern the hot_params forward runs) — otherwise a
        // scope added after an `arb.config.set` would stamp instance
        // accounts until the next set. `set_arb_leg_config` owns the
        // per-scope projection (the wired scope gets the map, a separate
        // deployment keeps its own book).
        let joiner_cfg = self.arb_leg_config.clone();
        let joiner_account = account_id.to_string();
        let scope_cfg = match (&joiner_cfg, joiner_account.as_str()) {
            (Some(c), acct) if acct == c.leg_a_account.trim() || acct == c.leg_b_account.trim() => {
                Some(c.clone())
            }
            (Some(c), acct) => {
                let mut projected = c.clone();
                projected.leg_a_account = acct.to_string();
                projected.leg_b_account = acct.to_string();
                Some(projected)
            }
            (None, _) => None,
        };
        hosted.strategy.set_arb_leg_config(scope_cfg);
        self.strategies.push(hosted);
        Ok(name)
    }

    /// Where a registered strategy came from (`dylib:<path>`, `test`, ...) —
    /// free-form provenance the loader supplies. The kernel itself has no
    /// `builtin` class any more.
    pub fn strategy_source(&self, name: &str) -> Option<&str> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.source.as_str())
    }

    /// Remove a strategy from the live dispatch (E9-b `strategy.unload` /
    /// `strategy.reload`).
    ///
    /// A registered strategy is DYNAMIC by construction — the kernel ships
    /// none — so every registered name is removable. The guard refuses while the
    /// strategy is ENABLED: an operator must disable it first, so a
    /// still-trading strategy is never silently pulled from under a running
    /// session. Returns `Err` with the reason on any refusal, `Ok(true)` when
    /// removed, `Ok(false)` when the name is unknown. An UNLOAD also drops
    /// the sticky provenance — the package is gone from the process, the
    /// name must stop being assignable (an assignment rebuild would find no
    /// source and warn-skip).
    pub fn unregister_user_strategy(&mut self, name: &str) -> Result<bool, String> {
        let Some(idx) = self
            .strategies
            .iter()
            .position(|s| s.strategy.name() == name)
        else {
            return Ok(false);
        };
        if self.strategies[idx].enabled {
            return Err(format!(
                "strategy {name} is ENABLED — disable it first (strategy.enable {name} false)"
            ));
        }
        self.strategies.remove(idx);
        self.package_sources.remove(name);
        Ok(true)
    }

    /// Remove EVERY scope instance of `name` while keeping the package's
    /// provenance (this is the ASSIGNMENT teardown — the package is still
    /// loaded in the process and a future assignment must be able to
    /// re-instantiate it; only `unregister_user_strategy` — the physical
    /// unload — drops the source).
    pub fn unregister_strategy_all_scopes(&mut self, name: &str) -> Result<bool, String> {
        if self
            .strategies
            .iter()
            .any(|s| s.strategy.name() == name && s.enabled)
        {
            return Err(format!(
                "strategy {name} is ENABLED — disable it first (strategy.enable {name} false)"
            ));
        }
        let before = self.strategies.len();
        self.strategies.retain(|s| s.strategy.name() != name);
        Ok(self.strategies.len() != before)
    }

    /// Scope instances of one strategy (account ids, registration order).
    pub fn strategy_scopes(&self, name: &str) -> Vec<String> {
        self.strategies
            .iter()
            .filter(|s| s.strategy.name() == name)
            .map(|s| s.account_id.clone())
            .collect()
    }

    /// issue 561 账户维重置：全部 (account, strategy) scope 对，注册序。
    /// 重置分支据此枚举目标账户的 scopes（停用其已启用者），不需要预先
    /// 知道策略名 —— 全量对先拿到再说。
    pub fn strategy_scopes_all(&self) -> Vec<(String, String)> {
        self.strategies
            .iter()
            .map(|s| (s.account_id.clone(), s.strategy.name().to_string()))
            .collect()
    }

    /// The (account, strategy) pairs currently demoted exits-only — the
    /// held-position safety net's live state, for diagnostics and tests.
    pub fn exits_only_scopes(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .strategies
            .iter()
            .filter(|s| s.exits_only)
            .map(|s| (s.account_id.clone(), s.strategy.name().to_string()))
            .collect();
        v.sort();
        v
    }

    /// Is `name` assignable? Every package LOADED this session (live scopes
    /// included, torn-down scopes' sources retained). The assignable-set
    /// check `account.strategies.set` runs plan-first against this.
    pub fn strategy_registered(&self, name: &str) -> bool {
        self.package_sources.contains_key(name)
    }

    /// (strategy name → source string) for every package LOADED this
    /// session, first registration wins. The host reads this BEFORE opening
    /// its engine borrow so the scope-builder closure can re-instantiate
    /// packages without touching engine state. Sources survive scope
    /// teardowns — a dropped scope's strategy stays re-assignable.
    pub fn scope_sources(&self) -> Vec<(String, String)> {
        self.package_sources
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect()
    }

    /// Rebuild one account's scope set from its strategy ENABLEMENT set
    /// (账户↔venue 解耦语义: `requested` is the account's 启用集 — the
    /// persisted 启用按钮 state — not an assignment or a visibility filter).
    /// Three phases, in this order:
    ///
    /// 1. DEMOTE: a scope of this account whose strategy is not in
    ///    `requested` but which still HELD open positions (`held`) flips to
    ///    exits-only — it contributes no entry candidates from the next
    ///    cycle on, its exit intents keep flowing until the account is flat.
    /// 2. DROP: a scope whose strategy is out of the set and flat (not in
    ///    `held`) is removed outright — its Lua state dies with it.
    /// 3. ADD: every requested strategy without a scope for this account is
    ///    instantiated through `build` (the host's Lua re-loader; `None`
    ///    means the source is not rebuildable — a dylib or vanished dir —
    ///    and the name is SKIPPED with a warning, never a panic) and arrives
    ///    ENABLED.
    ///
    /// Returns the names kept as exits-only (phase 1), for the reply.
    pub fn rebuild_account_scopes(
        &mut self,
        account: &str,
        requested: &[String],
        held: &[String],
        build: &mut ScopeBuilder<'_>,
    ) -> Vec<String> {
        let mut kept_exits_only: Vec<String> = Vec::new();
        // 1. Demote removed-and-held.
        for s in &mut self.strategies {
            if s.account_id != account {
                continue;
            }
            let name = s.strategy.name().to_string();
            if !requested.contains(&name) && held.contains(&name) {
                if !s.exits_only {
                    s.exits_only = true;
                }
                kept_exits_only.push(name);
            }
        }
        // 2. Drop removed-and-flat.
        //
        // The enablement write is a DECISION: a scope the request omits and
        // no position holds is torn down outright. Durable exposure is
        // covered by the DEMOTE phase above — a held strategy keeps managing
        // its exits until the account is flat.
        self.strategies.retain(|s| {
            if s.account_id != account {
                return true;
            }
            let name = s.strategy.name();
            requested.iter().any(|r| r == name) || held.iter().any(|h| h == name)
        });
        // 3. Add newly-requested scopes.
        for name in requested {
            match self
                .strategies
                .iter_mut()
                .find(|s| s.account_id == account && s.strategy.name() == *name)
            {
                Some(existing) => {
                    // A scope BACK in the enablement set returns to normal:
                    // the demotion clears AND the scope enables — being named
                    // in the set IS the 启用 state (the same rule the freshly
                    // built scope below gets). A pre-existing scope the boot
                    // registered DISABLED (no state-file replay, a fresh
                    // package) therefore turns on exactly when the file's
                    // enablement set names it.
                    existing.exits_only = false;
                    existing.enabled = true;
                }
                None => {
                    let Some(built) = build(name, account) else {
                        tracing::warn!(
                            strategy = %name,
                            account = %account,
                            "scope rebuild: source not rebuildable — scope skipped"
                        );
                        continue;
                    };
                    if let Err(reason) = self.register_scoped_strategy(
                        built,
                        format!("lua:scope:{account}"),
                        account,
                    ) {
                        tracing::warn!(
                            strategy = %name,
                            account = %account,
                            %reason,
                            "scope rebuild: scope registration refused"
                        );
                        continue;
                    }
                    // Being named in an account's enablement set is the
                    // operator's intent to run it there, so a rebuilt scope
                    // arrives ENABLED. The old rule inherited the default
                    // scope's global flag — an account given a
                    // globally-disabled strategy then silently never traded,
                    // with no error anywhere (生产 probe2 实锤: market_maker
                    // /mad_dog assigned but dead while spread_arb ran).
                    for s in &mut self.strategies {
                        if s.account_id == account && s.strategy.name() == *name {
                            s.enabled = true;
                        }
                    }
                }
            }
        }
        kept_exits_only
    }
}
