//! Split out of the parent module (issue #780): pure move.

use super::*;

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        // ZERO strategies: the kernel has no builtins (PR-B). Anything that
        // trades is registered by the host through the C ABI v2 loader.
        let strategies = Vec::new();
        Self {
            scanner: Scanner::new(cfg.scanner.clone()),
            books: HashMap::new(),
            spot: HashMap::new(),
            pending_tokens: HashSet::new(),
            last_candidates: Vec::new(),
            candidates_stable: false,
            last_time_left_sec: 0,
            last_blocked: Vec::new(),
            last_exemptions: Vec::new(),
            near_miss: crate::shadow::NearMissRecorder::new(500, 300_000),
            blocked_timing_total: 0,
            blocked_momentum_total: 0,
            gate_tally: HashMap::new(),
            gate_tally_acc: HashMap::new(),
            hot_params: None,
            strategies,
            package_sources: std::collections::BTreeMap::new(),
            strategy_exits: Vec::new(),
            positions_view: Vec::new(),
            pair_views_snapshot: Vec::new(),
            cfg,
            equity_usd: Decimal::ZERO,
            equity_by_account: HashMap::new(),
            trading_by_account: HashMap::new(),
            frozen_by_account: HashMap::new(),
            venue_by_account: HashMap::new(),
            size_pct_skipped: 0,
            klines: crate::kline::KlineAggregator::with_default_intervals(),
            unified_mappings: std::collections::BTreeMap::new(),
            arb_leg_config: None,
            pair_declarations: Vec::new(),
            pair_refusals: Vec::new(),
        }
    }

    /// RFC 568 阶段一：排空本周期收集的配对声明（Core 在 arb_precheck 过闸
    /// 后消费；每次真实评估前由 evaluate 清空）。
    pub fn take_pair_declarations(&mut self) -> Vec<PairDeclaration> {
        std::mem::take(&mut self.pair_declarations)
    }

    /// RFC 568 阶段一：排空本周期 A≠B 校验的拒绝回执（补钉 5；Core 转
    /// APPLICATION 错误事件）。
    pub fn take_pair_refusals(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pair_refusals)
    }

    /// 跨所套利弹窗 v2: hot-push the arb leg configuration (the host calls it
    /// on `arb.config.set`/`clear` and at registration). Fanned to every
    /// hosted strategy's adapter — the stamp source AND the params bag update
    /// in the same call, so a set is fully in force on the next evaluation.
    ///
    /// The fan-out is a PER-SCOPE PROJECTION, not a verbatim broadcast: a
    /// scope whose instance account IS one of the configured leg accounts
    /// receives the map as the user spelled it — its legs stamp per venue.
    /// Any other scope of the same package is a SEPARATE deployment (a
    /// second account running the package independently, the account-decouple
    /// posture): it keeps its own book whole — its projection rewrites BOTH
    /// leg accounts onto its own instance id, so its pair fills under its
    /// scope account exactly as the pre-v2 shape did, and it can never
    /// double-place legs onto the first deployment's accounts. Unconfigured
    /// (= None) fans out as None everywhere.
    pub fn set_arb_leg_config(&mut self, cfg: Option<crate::account::CrossVenueArbConfig>) {
        self.arb_leg_config = cfg.clone();
        for s in &mut self.strategies {
            let scope_cfg = match (&cfg, s.account_id.as_str()) {
                (Some(c), acct)
                    if acct == c.leg_a_account.trim() || acct == c.leg_b_account.trim() =>
                {
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
            s.strategy.set_arb_leg_config(scope_cfg);
        }
    }

    /// The arb leg configuration in force (diagnostics/tests read the engine
    /// mirror; the Core cell remains the wire truth).
    pub fn arb_leg_config(&self) -> Option<&crate::account::CrossVenueArbConfig> {
        self.arb_leg_config.as_ref()
    }

    /// #427: record one confirmed cross-venue mapping (last write wins per
    /// event id). Called by the host when a `UnifiedMapping` event arrives.
    pub fn put_unified_mapping(&mut self, event: blitzkrieg_market_api::unified::UnifiedEvent) {
        self.unified_mappings.insert(event.id.clone(), event);
    }

    /// ④: drop one confirmed mapping (operator revocation). The Engine holds
    /// only `put` semantics — the host removes the copy here on
    /// `unified.revoke` so strategies stop seeing the event immediately.
    /// Removing an unknown id is a no-op.
    pub fn remove_unified_mapping(&mut self, event_id: &str) {
        self.unified_mappings.remove(event_id);
    }

    /// #427: the confirmed cross-venue mappings, event-id ordered. The read
    /// view behind `bk.unified_events()` — an immutable snapshot the strategy
    /// layer assembles its cross-venue legs from.
    pub fn unified_mappings(
        &self,
    ) -> &std::collections::BTreeMap<String, blitzkrieg_market_api::unified::UnifiedEvent> {
        &self.unified_mappings
    }

    /// The account equity the equity-relative sizing (#202) is a percentage of
    /// — the host pushes it from the ledger before each evaluation, so the
    /// ticket follows the account it is actually trading. A non-positive value
    /// means "unknown": a leg with `size_pct > 0` then emits nothing, because
    /// sizing against an unknown account has no safe reading, and guessing the
    /// absolute budget back in would silently restore the fixed lot #202 is
    /// about.
    pub fn set_equity_usd(&mut self, equity: Decimal) {
        self.equity_usd = equity;
    }

    /// Push one account scope's book. The host calls this for EVERY account
    /// with a strategy assignment before each evaluation (the default account
    /// included — its value mirrors `set_equity_usd`). A scope without a push
    /// sizes against zero: fail-closed, no ticket.
    pub fn set_account_equity(&mut self, account: &str, equity: Decimal) {
        self.equity_by_account.insert(account.to_string(), equity);
    }

    /// Mirror one account's trading switch (account book fact). OFF = the
    /// scope contributes no entry candidates; its exits keep flowing.
    pub fn set_account_trading(&mut self, account: &str, enabled: bool) {
        self.trading_by_account.insert(account.to_string(), enabled);
    }

    /// Mirror one account's freeze (account-scoped kill switch). A frozen
    /// scope behaves exactly like a trading-OFF one — entries dropped,
    /// exits flowing — so a venue outage that freezes account A no longer
    /// stops account B from trading. Service-owned fact, engine only reads.
    pub fn set_account_frozen(&mut self, account: &str, frozen: bool) {
        self.frozen_by_account.insert(account.to_string(), frozen);
    }

    /// Mirror one account's venue binding (`account.venue.set`). Empty string
    /// = unbound (may trade any venue's tokens).
    pub fn set_account_venue(&mut self, account: &str, venue: &str) {
        self.venue_by_account
            .insert(account.to_string(), venue.to_string());
    }

    /// The equity a scope's sizing reads: the account's own pushed book for
    /// non-default scopes, the legacy scalar for the default scope.
    pub(super) fn scope_equity(&self, account: &str) -> Decimal {
        if account == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
            self.equity_usd
        } else {
            self.equity_by_account
                .get(account)
                .copied()
                .unwrap_or(Decimal::ZERO)
        }
    }

    /// Whether a scope may emit entry candidates this cycle. The default
    /// scope has no account-book switch (always true); every other scope
    /// reads the mirrored `trading_by_account` fact — an unknown account
    /// counts as OFF (fail-closed).
    pub(super) fn scope_trading(&self, account: &str) -> bool {
        if account == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
            return true;
        }
        self.trading_by_account
            .get(account)
            .copied()
            .unwrap_or(false)
    }

    /// Whether a scope is account-frozen (account-scoped kill switch). The
    /// default scope reads the same map: a manual `account.freeze default`
    /// must stop it too, so unlike `scope_trading` there is no privileged
    /// default arm. Unknown account = not frozen.
    pub(super) fn scope_frozen(&self, account: &str) -> bool {
        self.frozen_by_account
            .get(account)
            .copied()
            .unwrap_or(false)
    }

    /// Whether THIS (strategy, account) instance is demoted exits-only. The
    /// demotion is per instance: the default scope is never demoted (it is
    /// not assignment-managed), and a non-default instance only when a
    /// rebuild left it hosted for its held positions.
    pub(super) fn scope_exits_only(&self, strategy: &str, account: &str) -> bool {
        if account == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
            return false;
        }
        self.strategies
            .iter()
            .any(|s| s.account_id == account && s.strategy.name() == strategy && s.exits_only)
    }

    /// Whether a scope's venue binding admits a token: unbound admits
    /// everything; bound scopes their OWN venue's dual-key rows (RFC-661 M1).
    ///
    /// RFC-661 M1: the table is (venue, token) dual-keyed — each venue holds
    /// its OWN row for a byte-identical token — so a bound scope reads ITS
    /// OWN venue's row (`market_for_token_on_venue`) and admits iff that row
    /// exists. Insertion order is irrelevant (the PM-first/sim-first
    /// symmetry is test-pinned). A token the whole table does not carry is
    /// refused for bound scopes exactly as before; untagged rows (the
    /// legacy bucket, venue "" — the #635 pre-tag corpus case) still admit
    /// every bound scope through the token-only unique-candidate read, and
    /// a TAGGED row of another venue refuses (RFC-661 M3 retired the mirror
    /// bridge that used to overrule this verdict).
    /// Crate-visible so the IPC runtime-enablement tests can pin the wire
    /// behaviour the admission gate actually answers.
    pub(crate) fn scope_admits_token(&self, account: &str, token: &str) -> bool {
        let Some(bound) = self.venue_by_account.get(account).filter(|v| !v.is_empty()) else {
            return true;
        };
        if let Some(m) = self.scanner.market_for_token_on_venue(bound, token) {
            return m.venue.is_empty() || m.venue == *bound;
        }
        match self.scanner.market_for_token(token) {
            None => false,
            Some(m) => m.venue.is_empty(),
        }
    }

    /// The open-position snapshot the NEXT evaluation's ctx carries (the pair
    /// gap-fill surface). The host pushes the full list — every strategy, the
    /// shared PositionManager's current book — and `evaluate` slices it per
    /// strategy, so one strategy never sees another's positions. Stale between
    /// pushes by design: a cycle's view is the book as of its own start, the
    /// same consistency the books and equity in that ctx have.
    pub fn set_positions_view(&mut self, positions: Vec<HeldPosition>) {
        self.positions_view = positions;
    }

    /// RFC 568 阶段一：push this cycle's pair-book views（`bk.arb.pairs()`
    /// 的数据源）。The host pushes the full list — Core 已按策略×账户白名单
    /// 过滤——`evaluate` 再按 owner 策略切片进各自的 ctx。Stale between
    /// pushes by design（最终一致，附注 2）：a cycle's view is the book as
    /// of its own start.
    pub fn set_pair_views_snapshot(&mut self, views: Vec<crate::pair::PairView>) {
        self.pair_views_snapshot = views;
    }

    pub fn equity_usd(&self) -> Decimal {
        self.equity_usd
    }

    /// Signals skipped because an equity-relative budget could not buy one
    /// whole share (#202).
    pub fn size_pct_skip_count(&self) -> u64 {
        self.size_pct_skipped
    }

    /// The per-entry budget in USD that is actually in force for one strategy:
    /// `equity × pct%` scaled by the leg's weight when the relative mode is
    /// armed, else the absolute `size_usd`. Reported (panel + the sizing gate)
    /// so "what will this account commit per entry?" is a number, not a
    /// reading of the source.
    ///
    /// Scope note: this reads the `set_equity_usd` scalar, which the host
    /// keeps pinned to the ACTIVE account's ledger — this is the panel's
    /// active-account budget view. Actual ticket sizing is scope-correct
    /// regardless: `equity_shares` sizes each candidate against that
    /// candidate's own account equity, never this scalar.
    pub fn entry_budget_usd(&self, strategy: &str) -> Decimal {
        let sizing = self.effective_sizing(strategy);
        if sizing.size_pct <= Decimal::ZERO {
            return sizing.size_usd;
        }
        let raw = self.equity_usd.max(Decimal::ZERO) * sizing.size_pct / Decimal::ONE_HUNDRED;
        match sizing.size_weight {
            Some(w) => (raw * w).max(Decimal::ZERO).min(raw),
            None => raw,
        }
    }

    pub fn scanner(&self) -> &Scanner {
        &self.scanner
    }

    /// The scanner market a token belongs to (either side), if any. Multi-plugin
    /// callers (the rollover taker-kill) need the VENUE of a leg's token.
    ///
    /// RFC-661 M1: token-only resolution admits ONLY a unique candidate — the
    /// whole table holds exactly one row carrying the token. Ambiguity (≥2
    /// venues each hold a row) refuses with `None`, never first-hit by
    /// insertion order. Callers that know their venue read
    /// [`Self::market_for_token_on_venue`].
    pub fn market_for_token(&self, token: &str) -> Option<&crate::model::CryptoMarket> {
        self.scanner.market_for_token(token)
    }

    /// RFC-661 M1: the venue-qualified market lookup — `venue`'s OWN row for
    /// `token`. The same-byte token under two venues is two instruments; each
    /// resolves to itself. `None` when this venue holds no row for the token
    /// (even if a sibling venue does).
    pub fn market_for_token_on_venue(
        &self,
        venue: &str,
        token: &str,
    ) -> Option<&crate::model::CryptoMarket> {
        self.scanner.market_for_token_on_venue(venue, token)
    }

    /// The venues whose table rows carry `token` (sorted) — the ambiguity
    /// diagnostic for token-only callers that must name their candidates.
    pub fn venues_holding_token(&self, token: &str) -> Vec<String> {
        self.scanner.venues_holding_token(token)
    }

    pub fn set_config(&mut self, cfg: EngineConfig) {
        self.scanner.set_config(cfg.scanner.clone());
        // RFC-645 A-2 (决议④): the ONE marshal runs HERE, at the dispatch
        // site — every hosted strategy receives the same projection bytes the
        // foreign dylibs have always received (one build per set_config call,
        // shared by reference across the fan-out).
        let config_json = engine_strategy_config_json(&cfg.trend, &cfg.spread_arb);
        for s in &mut self.strategies {
            s.strategy.on_config(&config_json);
        }
        self.cfg = cfg;
    }

    /// Move the per-entry share band, in memory, without a restart (#191).
    ///
    /// Deliberately NOT `set_config`: that one re-notifies every hosted strategy
    /// through `on_config`, a side effect no sizing change asked for. The band is
    /// read live out of `cfg` on every ticket (`compute_shares` →
    /// `effective_sizing` → `global_sizing`), so writing these two fields is the
    /// whole change — and it remains the global ceiling that per-strategy
    /// overrides are clamped by, which is what makes it safe to move at runtime.
    pub fn set_share_band(&mut self, min_shares: Decimal, max_shares: Decimal) {
        self.cfg.min_shares = min_shares;
        self.cfg.max_shares = max_shares;
    }

    /// Tokens whose trend is currently confirmed (diagnostics/tests). The union
    /// over registered strategies.
    pub fn confirmed_tokens(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for s in &self.strategies {
            out.extend(s.strategy.confirmed_tokens());
        }
        out
    }

    /// Diagnostics: for each confirmed token, its current mid and whether it
    /// currently satisfies the entry band (per strategy).
    pub fn confirmed_diagnostics(&self, now_ms: i64) -> Vec<serde_json::Value> {
        // E3: the diagnostic ctx's round clock resolves on the TABLE's own
        // family — every row through `row_family_duration`, read on the legacy
        // venue bucket exactly like the global clock. No candidate exists at
        // this call (diagnostics walks strategies, not signals), so the table
        // shape is the best family fact here; a single-family table — every
        // corpus, and the per-family boot instances — resolves to ONE
        // duration, byte-identical with the old global read.
        let round = self.table_round_timing(now_ms);
        let fresh =
            |token: &str| fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms);
        let ctx = StrategyCtx::new(
            self.scanner.markets(),
            round.slot,
            round.time_left_sec,
            now_ms,
            &fresh,
        );
        self.strategies
            .iter()
            .flat_map(|s| s.strategy.diagnostics(&ctx))
            .collect()
    }

    pub fn book_snapshot(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        self.books
            .get(token_id)
            .filter(|b| !b.is_empty())
            .map(|b| b.snapshot(token_id))
    }

    /// Feed one market-data event. Returns tokens whose confirmed trend broke so
    /// the caller can cancel their resting bids.
    pub fn on_data(&mut self, ev: DataEvent) -> Vec<(String, Decimal)> {
        match ev {
            DataEvent::Book {
                token_id,
                bids,
                asks,
                now_ms,
            } => {
                // The book for an in-flight token almost always exists already
                // (markets arrive before their books); a get_mut hit avoids the
                // owned-key String that `entry` would allocate per event.
                let b = match self.books.get_mut(&token_id) {
                    Some(b) => b,
                    None => self.books.entry(token_id.clone()).or_default(),
                };
                b.apply_snapshot(&bids, &asks, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
                self.feed_klines(&token_id, &snap, now_ms);
            }
            DataEvent::Books { .. } => {
                // 盘口批次原子化：批事件不在本 match 里展开 —— 撕裂的根就是
                // 逐 token 的独立 apply。归档回放（`k:"books"` 批行 →
                // `event_from_json` → 本函数）与任何直接喂入都拿到与
                // [`Engine::on_data_books`] 同一条整批路径：一次调用顺序应用，
                // 策略永远看到同一批快照。静默吞掉（旧实现）会让回放里整批
                // tick 消失——比撕裂更坏的假象。
                let books = match ev {
                    DataEvent::Books { books, now_ms } => (books, now_ms),
                    _ => unreachable!("matched Books arm"),
                };
                let (books, now_ms) = books;
                return self.on_data_books(&books, now_ms);
            }
            DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                now_ms,
            } => {
                let b = self.books.entry(token_id.clone()).or_default();
                b.update_top(best_bid, best_ask, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
                self.feed_klines(&token_id, &snap, now_ms);
            }
            DataEvent::Spot {
                asset,
                price,
                now_ms,
            } => {
                self.spot
                    .entry(asset.clone())
                    .or_insert_with(|| PriceBuffer::new(180))
                    .push(price, now_ms);
                self.feed_kline_price(&asset, price, now_ms);
                return Vec::new();
            }
            DataEvent::RoundMarkets { markets, now_ms } => {
                // #498: a venue can list the round's markets STAGGERED — Kalshi's
                // four crypto assets open minutes apart, and a discovery that
                // locks the slot on its first non-empty tick then feeds 2 of 4.
                // The late listings arrive as a SECOND RoundMarkets for the SAME
                // round_slot: a full rollover replay here would wipe
                // `pending_tokens` (re-arming duplicate entries mid-round) and
                // restart the candidates-stable window for nothing. So a
                // same-slot event MERGES: append the markets the scanner does
                // not carry yet and hand the strategy layer only the genuinely
                // new books — no `on_round` re-fire, no pending/candidate
                // reset, no clock change (the first feed already observed the
                // round's one expiry). A different slot is a real rollover and
                // keeps the full replay below.
                //
                // Multi-plugin data plane: the check is VENUE-BLIND slot
                // membership (`.first()` generalized to `.any()`) — in a
                // mixed-venue table the scanner's first market may be any
                // venue's, and a same-slot feed from a SECOND venue is exactly
                // the same staggered-listing top-up (#498's "迟上架轮内补查",
                // now across venues). Single-venue tables hold one slot, so
                // `any` == the old `first` check — #498's corpus and the
                // pre-venue baselines stay byte-identical.
                //
                // B1c: slot numbers are only comparable WITHIN one duration
                // family — the 900s grid's slot 5 and the 3600s grid's slot 5
                // are two different wall-clock rounds (9000s apart). The
                // membership test is therefore (slot, duration) equality: the
                // feeding batch's own declared `round_duration_sec` (0 =
                // legacy archive → the configured global grid) must match the
                // resident row's. Single-family deployments resolve every row
                // and every batch to the SAME duration, so the added
                // condition is constant-true there and the verdict is
                // bit-identical with the pre-B1c slot-only check.
                let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
                let feeding_venue = markets.first().map(|m| m.venue.clone()).unwrap_or_default();
                let global_duration_sec = self.cfg.scanner.round_duration_sec;
                let feed_duration = row_family_duration(
                    markets.first().map(|m| m.round_duration_sec).unwrap_or(0),
                    global_duration_sec,
                );
                // 多场馆同格网（2026-10-25 poly 轮钟冻结定谳）：每个插件的
                // discovery 按自己的节奏喂自己的轮。一类批次必须丢：**slot
                // 落后于同族驻留 slot、且自身窗口已经过期**——那是「迟到喂的
                // 已过期轮」，收下它会把整张表拖回旧轮，抹掉另一个 venue 的
                // discovery 已经落好的新轮行（A 场馆迟喂过期轮 ↔ B 场馆喂
                // 新轮，ping-pong 互相覆写，轮钟冻在 00:00）。只落后但窗口
                // 还活着的批次要收：未来档先落、当前交易轮后到是生产 discovery
                // 的正常形状（对抗性喂序测试钉的就是它）。同 slot 走下面的
                // 合流，更新 slot 走下面的 rollover。单族语料 slot 单调且
                // 批次窗口未过期，此守卫恒不触发——回放逐字节不变。
                let resident_family_slot = self
                    .scanner
                    .markets()
                    .iter()
                    .filter(|m| {
                        row_family_duration(m.round_duration_sec, global_duration_sec)
                            == feed_duration
                    })
                    .map(|m| m.round_slot)
                    .max()
                    .unwrap_or(0);
                let batch_expired = markets
                    .iter()
                    .map(|m| m.expires_at_ms)
                    .min()
                    .map(|end| end <= now_ms)
                    .unwrap_or(false);
                if resident_family_slot > slot && batch_expired {
                    tracing::warn!(
                        venue = %feeding_venue,
                        batch_slot = slot,
                        resident_slot = resident_family_slot,
                        "stale round batch dropped — an expired slot behind the resident family"
                    );
                    return Vec::new();
                }
                let scanner_holds_slot = slot != 0
                    && self.scanner.markets().iter().any(|m| {
                        m.round_slot == slot
                            && row_family_duration(m.round_duration_sec, global_duration_sec)
                                == feed_duration
                    });
                if scanner_holds_slot {
                    // RFC-661 M1: the known-token test is VENUE-QUALIFIED —
                    // a token is "already known" only when the FEEDING
                    // VENUE's own rows already carry it. A sibling venue's
                    // byte-identical token is a DIFFERENT instrument (the
                    // dual-key table holds both rows), so its batch is a
                    // fresh batch for its own venue, not a duplicate of the
                    // first venue's. #627's token-unique invariant (and the
                    // #638 fresh-empty merge that hid behind it) is
                    // superseded here: two venues' same-slot batches each
                    // land their own rows, whichever arrives first.
                    let batch_venue_tag =
                        markets.first().map(|m| m.venue.clone()).unwrap_or_default();
                    let batch_first_expiry =
                        markets.first().map(|m| m.expires_at_ms).unwrap_or_default();
                    let known: std::collections::HashSet<&str> = self
                        .scanner
                        .markets()
                        .iter()
                        .filter(|m| m.venue == batch_venue_tag)
                        .flat_map(|m| [m.up_token_id.as_str(), m.down_token_id.as_str()])
                        .collect();
                    // RFC-661 M1: "known" is judged against the feeding
                    // VENUE's rows AT THE BATCH'S OWN SLOT. The venue's stale
                    // rows from a PREVIOUS window carry the same tokens (a
                    // binary market re-lists both sides every round), so a
                    // slot-blind known set would misread this venue's own
                    // ROLLOVER as a duplicate re-feed and freeze its rows at
                    // the old round forever (the poly slot-2 row beside the
                    // sibling's slot-3 row — found in the first A/B replay).
                    // The batch's slot and the resident rows' slot therefore
                    // both key the comparison: same (venue, slot) + all
                    // tokens known = duplicate; anything else = fresh rows
                    // for the feeding venue.
                    let batch_has_new_slot = !self
                        .scanner
                        .markets()
                        .iter()
                        .filter(|m| m.venue == batch_venue_tag)
                        .all(|m| m.round_slot == slot);
                    let fresh: Vec<CryptoMarket> = if batch_has_new_slot {
                        markets.clone()
                    } else {
                        markets
                            .iter()
                            .filter(|m| {
                                !known.contains(m.up_token_id.as_str())
                                    && !known.contains(m.down_token_id.as_str())
                            })
                            .cloned()
                            .collect()
                    };
                    if fresh.is_empty() {
                        // Pure duplicate: THIS venue re-feeding its own
                        // round. Nothing to add, nothing to restart; the
                        // (venue, duration) clock re-speaks (observe is
                        // idempotent for a same-venue re-declaration — the
                        // same value rewrites the same key, state
                        // byte-identical). #550: the re-speak IS the clock
                        // opening for this (venue, duration) family — a
                        // duplicate re-feed whose clock never spoke would
                        // strand the panel countdown at 00:00 on an account
                        // switch. RFC-661 M1: this arm is reachable ONLY for
                        // a same-venue re-feed now — a sibling venue's batch
                        // (byte-identical tokens included) carries fresh
                        // tokens for ITS venue and lands its own rows below;
                        // the dual-key table carries every venue's rows
                        // naturally (the #638 display book is gone).
                        self.scanner.observe_end_time_for(
                            &batch_venue_tag,
                            feed_duration,
                            batch_first_expiry,
                            now_ms,
                        );
                        return Vec::new();
                    }
                    // Seed the genuinely new tokens from any book that has
                    // already arrived (the descriptors usually precede their
                    // first book event) BEFORE the scanner grows: the borrow
                    // of the merged list ends with `set_markets`, and the
                    // seeds only need `fresh`.
                    for t in fresh
                        .iter()
                        .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    {
                        if let Some(snap) = self.book_snapshot(&t) {
                            for s in &mut self.strategies {
                                s.strategy.on_book(&t, &snap, now_ms);
                            }
                        }
                    }
                    // Observe the feeding batch's own expiry on the merge path
                    // too. The comment above ("the first feed already observed
                    // the round's one expiry") is a single-venue assumption:
                    // in a mixed-venue table the staggered top-up is usually
                    // the SECOND venue's ONLY feed of the round — its first
                    // `RoundMarkets` lands here, never at the rollover — so
                    // skipping the observation would leave it out of
                    // `observed_venue_keys()` for the whole round: no panel
                    // `venueRounds` row (the countdown rebind waits for it and
                    // shows 00:00 on an account switch to that venue) and its
                    // timing gate reading the sibling's clock. Same discipline
                    // as the rollover arm: the declaration lands in ITS OWN
                    // venue slot and out-of-band values are refused inside
                    // `observe_end_time_for_venue`.
                    let merge_venue = fresh.first().map(|m| (m.venue.clone(), m.expires_at_ms));
                    // RFC-661 M1: a same-slot batch whose venue holds only
                    // STALE rows (its previous window) is that venue's
                    // rollover through the merge arm — the feeding venue's
                    // own family rows are replaced, not appended to (the
                    // appended shape left the old-round row beside the new
                    // one, growing the table every window).
                    let mut merged: Vec<CryptoMarket> =
                        if batch_has_new_slot && !batch_venue_tag.is_empty() {
                            let global = global_duration_sec;
                            let tag = batch_venue_tag.as_str();
                            self.scanner
                                .markets()
                                .iter()
                                .filter(|m| {
                                    m.venue != tag
                                        || row_family_duration(m.round_duration_sec, global)
                                            != feed_duration
                                })
                                .cloned()
                                .collect()
                        } else {
                            self.scanner.markets().to_vec()
                        };
                    merged.extend(fresh);
                    self.scanner.set_markets(merged);
                    if let Some((venue, end_ms)) = merge_venue {
                        // B1c: the declaration lands in the FEEDING FAMILY's
                        // (venue, duration) clock slot, not the global-grid
                        // slot — a 1h top-up must not speak into the 15m
                        // family's clock. Single-family feeds resolve
                        // `feed_duration` to the configured grid, the same
                        // key the legacy entry wrote.
                        self.scanner
                            .observe_end_time_for(&venue, feed_duration, end_ms, now_ms);
                    }
                    return Vec::new();
                }
                // ── #550 复现缺陷（症状①）的根因之一：无界 sibling 存活 ──
                // 换轮臂（下方）只替换 FEEDING venue 的行，兄弟行「存活到
                // 自己的 feed 来换」是 staggered 上架的桥接语义（兄弟在轮
                // 界刚过、还没重报的几秒里不能丢行），但没有边界：兄弟停喂
                // 后它的行带着已过期的 expiry 永久留在表里 —— 行情页渲染
                // 已结束轮次的行，它的 observed expiry 还通过全局轮钟的
                // `earliest_observed` 把 time_left 钉死 0（全体 Timing 门
                // 拦死新开仓）。
                //
                // 处置：错满一整轮（round_slot 距本批 slot ≥ 2）的兄弟行
                // 清出表；只差一格的照旧桥接（下方 per-venue 替换照常处置
                // feeding venue 自己的行，这里不重复动它）。venue-blind 的
                // merge 判定按 (slot, duration) 相等匹配（B1c），落后行永不
                // 命中 merge 路径，所以这不改变 merge/rollover 的判定本身
                // —— 只给「存活」加一条可退出的边界。
                //
                // B1c（#550 按族网格）：slot 数字跨 duration 族不可比 ——
                // 900s 网格的 slot N 与 3600s 网格的 slot N 相差数小时。
                // 「落后几格」的判定因此按每行自己的 duration 网格逐行算
                // （行带 round_duration_sec 用它，否则全局轮长），而不是
                // 一根全局尺。单族表里每行解析出同一个 duration，逐行判定
                // 与旧的全局判定逐字节等价。
                if slot != 0 {
                    let mut kept: Vec<CryptoMarket> = self
                        .scanner
                        .markets()
                        .iter()
                        .filter(|m| {
                            // B1c: the lag is measured on the ROW's OWN grid —
                            // the feeding round's start (slot × feed duration,
                            // seconds) converted onto that row's duration
                            // before the distance is taken. Slot numbers are
                            // not comparable across duration families (the
                            // 900s grid's slot 100 and the 3600s grid's slot
                            // 100 are hours apart); a 0-duration row (legacy
                            // archive) rides the global grid — the same
                            // resolution the merge test uses. Same-family rows
                            // (the only rows a single-family table holds)
                            // resolve to the historic `slot - round_slot`
                            // verbatim.
                            let row_dur =
                                row_family_duration(m.round_duration_sec, global_duration_sec)
                                    .max(1);
                            let lag = if row_dur == feed_duration {
                                slot - m.round_slot
                            } else {
                                slot * feed_duration / row_dur - m.round_slot
                            };
                            lag < 2 || m.venue == feeding_venue
                        })
                        .cloned()
                        .collect();
                    if kept.len() != self.scanner.markets().len() {
                        let dropped = self.scanner.markets().len() - kept.len();
                        tracing::info!(
                            target: "engine",
                            "round rollover: dropping {dropped} stale market row(s) from \
                             venue(s) that missed a full round (slot < {}), feeding venue \
                             {feeding_venue} excluded",
                            slot - 1
                        );
                        self.scanner.set_markets(std::mem::take(&mut kept));
                    }
                }
                // The venue's own round declaration beats the local slot grid:
                // `expires_at_ms` is the exchange's actual round end, and
                // `time_left_sec` (the exit policy's and the D-31 floor's
                // clock) must track it, not the host's wall clock. Without
                // this, a misaligned or shifted venue schedule silently
                // drifts every timing decision. `observe_end_time` ignores
                // non-positive declarations and derives the clock offset —
                // into the feeding market's OWN venue slot (multi-plugin:
                // Kalshi's declaration must not move Polymarket's clock).
                if let Some(m) = markets.first() {
                    // B1c: the feeding batch's declaration speaks for ITS OWN
                    // (venue, duration) family — a 1h rollover must not move
                    // the 15m family's clock.
                    self.scanner.observe_end_time_for(
                        &m.venue,
                        feed_duration,
                        m.expires_at_ms,
                        now_ms,
                    );
                }
                // Real round timing for the transition: how much of the round
                // was left at this instant, straight from the scanner. The
                // same-batch venue's own clock (a rollover REPLACES that
                // venue's markets — its old expiry is gone with them; the
                // conservative global window still covers the siblings).
                // B1c: the family's own clock — the feeding family's duration,
                // not the global grid, decides the slot boundary. In a
                // single-family deployment this is the legacy entry verbatim.
                let venue = feeding_venue.clone();
                let time_left_sec = self
                    .scanner
                    .round_timing_for(&venue, feed_duration, now_ms)
                    .time_left_sec;
                for s in &mut self.strategies {
                    s.strategy.on_round(slot, time_left_sec, now_ms);
                }
                // ── B1c：rollover 只重置 feeding (venue, duration) 族 ──
                // 这三条状态（pending 去重、staged 候选、稳定相位）是
                // 「这一轮的入场」的事实。升级前全局轮长一根尺：任何 venue
                // 换轮都全体清空 —— 单族正确；多族下 1h 轮的翻转会把 15m
                // 族轮内刚挂上的 pending 清掉（重新武装重复入场）并作废
                // 别族 staged 候选。scanner 侧喂钟（B1a）已按 (venue,
                // duration) 键控，engine 侧 reset 对齐同族：只有本族 token
                // /本族候选出局，别族不动。
                //
                // 本族 token 集：本批换轮后的新行 token ∪ 表里同族旧行
                // token —— 同 (venue,duration) 的全部行（不分 slot 数字：
                // 落后一兩格的同族兄弟行是 staggered 桥接里的旧轮行，其轮
                // 已换、其 pending 同样必须出局）就是「这个族的入场事实」
                // 的全部对象；别族（不同 duration）的行不在集合里，它们的
                // pending/staged 候选原样不动。喂入行无 venue 标签（legacy
                // 单插件）时整表就是本族，集合即全表 —— 与旧的全清逐字节
                // 等价。
                let family_tokens: HashSet<String> = {
                    let mut set: HashSet<String> = HashSet::new();
                    for m in &markets {
                        set.insert(m.up_token_id.clone());
                        set.insert(m.down_token_id.clone());
                    }
                    for m in self.scanner.markets() {
                        if row_family_duration(m.round_duration_sec, global_duration_sec)
                            == feed_duration
                        {
                            set.insert(m.up_token_id.clone());
                            set.insert(m.down_token_id.clone());
                        }
                    }
                    set
                };
                self.pending_tokens
                    .retain(|(_, tok)| !family_tokens.contains(tok));
                // Issue #390 (replay nofast): the new round retires every
                // staged candidate — the base's next collection ran against
                // the new market list and round clock, so a replayed set from
                // the old round must not leak into it. B1c: only candidates
                // of the FEEDING FAMILY retire — a sibling family's staged
                // set (its tokens still live, its clock still running) stays
                // staged; single-family tables lose everything, byte-identical
                // with the unscoped clear.
                self.last_candidates
                    .retain(|sig| !family_tokens.contains(&sig.token_id));
                // A set that survived the family cut is a candidate set the
                // base's next collection can still reproduce (its inputs —
                // the sibling family's books, clock and round list — did not
                // change with THIS family's rollover), so the stability flag
                // keeps describing it. The historic whole-clear emptied the
                // set, and an empty staged set was exactly "not stable"; the
                // empty-set verdict falls out naturally here too.
                self.candidates_stable = self.candidates_stable && !self.last_candidates.is_empty();
                // The rollover replaces the FEEDING VENUE's markets only: a
                // batch always lists the complete round of its own venue, so
                // everything of `venue` in the table is stale. Sibling venues'
                // markets (multi-plugin data plane) stay live — they roll over
                // through their own feeds. An empty venue tag keeps the whole
                // table replace (the historical single-plugin semantics).
                // B1c: "everything of the venue" means the venue's rows of the
                // FEEDING FAMILY — a same-venue sibling family (PM 15m rows
                // beside PM 1h rows) is live in its own round and is replaced
                // by its OWN feed, never collateral of this one. Single-family
                // tables resolve every row to the feed's duration, so the
                // predicate degenerates to the historic `m.venue != venue`.
                // RFC-661 M1: "the venue's rows" are the dual-key rows of THIS
                // venue only — a sibling venue's byte-identical token rows are
                // different instruments and must survive this venue's
                // rollover untouched (per-venue round clocks included).
                if venue.is_empty() {
                    self.scanner.set_markets(markets);
                } else {
                    let mut kept: Vec<CryptoMarket> = self
                        .scanner
                        .markets()
                        .iter()
                        .filter(|m| {
                            m.venue != venue
                                || row_family_duration(m.round_duration_sec, global_duration_sec)
                                    != feed_duration
                        })
                        .cloned()
                        .collect();
                    kept.extend(markets);
                    self.scanner.set_markets(kept);
                }
                // Seed trend with current mids so confirmation starts immediately.
                let now = now_ms;
                let tokens: Vec<String> = self
                    .scanner
                    .markets()
                    .iter()
                    .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    .collect();
                for t in tokens {
                    if let Some(snap) = self.book_snapshot(&t) {
                        for s in &mut self.strategies {
                            s.strategy.on_book(&t, &snap, now);
                        }
                    }
                }
            }
            DataEvent::RoundEnd { now_ms: _ } => {
                // #352: boundary marker only — the rollover that actually
                // replaces the markets is `RoundMarkets`; a replay consumer
                // uses RoundEnd to close the round's book. Nothing to trade on.
                return Vec::new();
            }
            DataEvent::Trade { .. } => {
                // #352: an on-chain print is information, not a book update —
                // strategies decide on books and spot only.
                return Vec::new();
            }
            DataEvent::Resolution { .. } => {
                // #377 (治 #2): a verdict is settlement input for the Core,
                // not a book update — no strategy acts on it here.
                return Vec::new();
            }
            DataEvent::Cashflow { .. } => {
                // #388: a wallet-level cash row is ledger input for the Core,
                // not a book update — no strategy acts on it here.
                return Vec::new();
            }
            DataEvent::UnifiedMapping { .. } => {
                // #427: a confirmed cross-venue mapping is strategy-layer
                // state (which tokens across venues are one bet), not a book
                // update. The core keeps the latest mapping per event id so
                // the strategy layer can read it through the view; the match
                // arm stays inert to trading.
            }
        }
        self.drain_breaks()
    }

    /// 盘口批次原子化：整批 book 在同一次调用里顺序应用，每一本走与
    /// [`DataEvent::Book`] 完全相同的 apply → 逐策略 `on_book` → near-miss →
    /// K-line 路径，策略因此永远看到同一批快照（时间戳一致、新旧一致），
    /// 不存在「批内一半新一半旧」的中间态。批次时间戳 `now_ms` 由调用方
    /// 统一给定（批式拉取一次请求一个时钟）。返回聚合的趋势断裂供调用方
    /// 撤单（与 [`Engine::on_data`] 的返回语义相同）。
    pub fn on_data_books(&mut self, books: &[BatchBook], now_ms: i64) -> Vec<(String, Decimal)> {
        for (token_id, bids, asks) in books {
            let b = match self.books.get_mut(token_id) {
                Some(b) => b,
                None => self.books.entry(token_id.clone()).or_default(),
            };
            b.apply_snapshot(bids, asks, now_ms);
            let snap = b.snapshot(token_id);
            for s in &mut self.strategies {
                s.strategy.on_book(token_id, &snap, now_ms);
            }
            self.near_miss.on_book(token_id, &snap, now_ms);
            self.feed_klines(token_id, &snap, now_ms);
        }
        self.drain_breaks()
    }

    /// E29 (§10.3): fold one book-derived price into the K-line aggregator.
    ///
    /// The venue publishes no trade tape this deployment can read, so the bar
    /// is built from the book MID at the event's own timestamp — the same
    /// price the exit policy and the trend filter already decide on, not a
    /// new feed and not a guess. One tick per `(symbol, interval)` per event;
    /// the aggregator owns bucketing, out-of-order drops and closing.
    ///
    /// `_now_ms` is deliberately the EVENT timestamp (`now_ms` carried by the
    /// DataEvent), not the host wall clock: K-line bucketing runs on venue
    /// time, the clock every other decision here already runs on.
    fn feed_klines(&mut self, symbol: &str, snap: &OrderbookSnapshot, ts_ms: i64) {
        self.feed_kline_price(symbol, snap.mid_price, ts_ms);
    }

    /// E29: fold one price tick (book mid or spot print) into the aggregator
    /// and dispatch the bars it closed. Non-positive prices are skipped: an
    /// empty or one-sided book has no honest price to fold.
    fn feed_kline_price(&mut self, symbol: &str, price: Decimal, ts_ms: i64) {
        if price <= Decimal::ZERO {
            return;
        }
        let closed = self.klines.on_trade(symbol, price, Decimal::ONE, ts_ms);
        if closed.is_empty() {
            return;
        }
        // §10.4: strategies see CLOSED bars only, in closure order.
        for s in &mut self.strategies {
            for bar in &closed {
                s.strategy.on_kline(bar);
            }
        }
    }

    /// E29: bars closed since the last drain — the host's `KLINE_UPDATE`
    /// source. Drained exactly once (§12.2: a closed bar MUST be pushed).
    pub fn take_closed_klines(&mut self) -> Vec<crate::kline::Kline> {
        self.klines.take_closed()
    }

    /// E29: the still-growing bars, for the throttled "正在长" push.
    pub fn kline_current_bars(&self) -> Vec<crate::kline::Kline> {
        self.klines.current_bars()
    }

    /// E29: `kline.history` body — closed tail + the growing bar last.
    pub fn kline_history(
        &self,
        symbol: &str,
        interval: crate::kline::KlineInterval,
        limit: usize,
    ) -> Vec<crate::kline::Kline> {
        self.klines.history(symbol, interval, limit)
    }

    /// E29: aggregation counters (drops must be visible, never silent).
    pub fn kline_stats(&self) -> crate::kline::AggregatorStats {
        self.klines.stats()
    }

    /// Collect trend breaks from every strategy (each break is cancelled once).
    ///
    /// Every hosted strategy is DRAINED, but only the enabled ones contribute. The
    /// hook still runs on a disabled strategy because its library may be queueing
    /// breaks internally and the call is what empties that queue — skipping it
    /// would let the queue grow for as long as the strategy stays off. The breaks
    /// are then dropped rather than returned: a break's only effect is cancelling
    /// this token's resting entry bids, and a disabled strategy must not change
    /// another strategy's fills (#302 — `trend_follow` registered-but-off still
    /// pulled `spread_arb`'s bids, so the fills depended on which libraries
    /// happened to sit in the strategy directory).
    fn drain_breaks(&mut self) -> Vec<(String, Decimal)> {
        let mut out = Vec::new();
        for s in &mut self.strategies {
            let breaks = s.strategy.take_breaks();
            if s.enabled {
                out.extend(breaks);
            }
        }
        out
    }

    /// Spot momentum alignment filter: reject when spot is moving against the bet.
    /// Returns the reason it rejected, so an exempting strategy's audit record can
    /// state exactly what was waived.
    pub(super) fn momentum_ok(
        &self,
        asset: &str,
        dir: SignalDirection,
        now_ms: i64,
    ) -> Result<(), String> {
        let Some(buf) = self.spot.get(asset) else {
            return Ok(());
        };
        let move_pct = buf.move_pct(self.cfg.momentum_window_sec, now_ms);
        let against = match dir {
            SignalDirection::Up => move_pct < -self.cfg.momentum_tol_pct,
            SignalDirection::Down => move_pct > self.cfg.momentum_tol_pct,
        };
        if against {
            return Err(format!(
                "spot {asset} {move_pct:+.2}% in {}s vs {} (tol {}%)",
                self.cfg.momentum_window_sec,
                dir.as_str(),
                self.cfg.momentum_tol_pct
            ));
        }
        Ok(())
    }

    /// Evaluate all enabled strategies against the current round and return the
    /// entry orders that should be placed. Pure w.r.t. the core: the caller
    /// places them and reports back tokens that got an order so repeats are
    /// suppressed this round.
    pub fn evaluate(&mut self, now_ms: i64) -> Vec<crate::model::OrderRequest> {
        self.last_blocked.clear();
        self.last_exemptions.clear();
        // RFC 568 阶段一：配对声明与拒绝回执都是「本周期」的伴生事实 ——
        // 每次真实评估重开（Core 在 precheck 过闸后消费 take_pair_declarations，
        // 拒绝回执经 take_pair_refusals 转 APPLICATION 事件）。
        self.pair_declarations.clear();
        self.pair_refusals.clear();

        // E3: the cycle's round clock reads the TABLE's own family (the
        // legacy venue fold, duration refined) instead of the configured
        // grid — single-family tables resolve to the same numbers, a mixed
        // table stops reporting 15m-grid slot/time_left to strategies
        // pricing other durations. The per-candidate refinements below use
        // each candidate's OWN market family.
        let round = self.table_round_timing(now_ms);
        // E25: the arbitration pipeline's physics projection reads the SAME
        // round timing this cycle was judged against.
        self.last_time_left_sec = round.time_left_sec;

        // Candidates are computed regardless of the timing gate so we can record
        // near-misses (a valid dip that the timing gate blocked). This is the data
        // the shadow log lacks and the --replay entry analysis needs.
        let mut candidates: Vec<TradeSignal> = Vec::new();
        let mut exit_intents: Vec<crate::strategies::StrategyExitIntent> = Vec::new();
        // #427: the mapping snapshot for this cycle — every strategy sees the
        // SAME confirmed events (state of the world, not per-strategy data).
        let unified_snapshot: Vec<blitzkrieg_market_api::unified::UnifiedEvent> =
            self.unified_mappings.values().cloned().collect();
        {
            let fresh = |token: &str| {
                fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms)
            };
            // The host-pushed position snapshot, sliced per scope inside the
            // loop: each scope's ctx carries ONLY its own positions — same
            // strategy on two accounts is two independent books, and neither
            // package may read the other's holdings through `bk.holdings()`.
            // 跨所套利弹窗 v2 exception: a strategy that trades under the arb
            // leg configuration STAMPS each leg to ITS OWN venue's account
            // (entries and exits alike), so one instance's book legitimately
            // spans the configured leg accounts — the release decision prices
            // the WHOLE pair and must see both legs' held rows, or a B-leg
            // exit would be suggested against a holding the instance cannot
            // see (the leg would be trapped until settlement). The widening
            // is bounded by the configuration itself: only rows whose account
            // is one of the two leg accounts join the view, so unrelated
            // scopes' books stay sealed.
            for s in self.strategies.iter_mut() {
                let leg_accounts = s.strategy.arb_leg_accounts();
                if s.enabled {
                    let held: Vec<HeldPosition> = self
                        .positions_view
                        .iter()
                        .filter(|p| {
                            p.strategy == s.strategy.name()
                                && (p.account_id == s.account_id
                                    || leg_accounts
                                        .as_ref()
                                        .is_some_and(|ls| ls.contains(&p.account_id)))
                        })
                        .cloned()
                        .collect();
                    // RFC 568 阶段一：同样的 per-scope 切片，配对簿视图 ——
                    // 只按 owner 策略切（账户白名单过滤 Core 侧已做完）。
                    let pair_views: Vec<crate::pair::PairView> = self
                        .pair_views_snapshot
                        .iter()
                        .filter(|p| p.owner_strategy == s.strategy.name())
                        .cloned()
                        .collect();
                    let ctx = StrategyCtx::new(
                        self.scanner.markets(),
                        round.slot,
                        round.time_left_sec,
                        now_ms,
                        &fresh,
                    )
                    .with_positions(&held)
                    .with_unified_events(&unified_snapshot)
                    .with_pair_views(&pair_views);
                    candidates.extend(s.strategy.find_candidates(&ctx));
                    // Close intents accumulate regardless of the entry timing
                    // gate: an open position's exit is never round-timing-gated.
                    exit_intents.extend(s.strategy.take_exit_intents());
                }
            }
        }
        // Issue #390 (replay nofast): stage what this real evaluation
        // produced, and decide the replay licence. The inputs behind the set
        // are event-driven, so between events it cannot change — a stable
        // set (identical to the previous real evaluation's) is the state a
        // quiet replay cycle reproduces; a changed set is a phase transition
        // (a latch fired, an emission was consumed) whose NEXT evaluation
        // must be real, because the base's next collection would collect the
        // post-transition output this cycle already produced.
        self.candidates_stable = self.last_candidates == candidates;
        self.last_candidates = candidates.clone();
        self.strategy_exits.extend(exit_intents);
        self.submit_candidate_orders(candidates, now_ms)
    }

    /// Issue #390 (replay nofast): a quiet-tick evaluation cycle. Runs the
    /// STANDING candidates of the most recent real evaluation through the
    /// SAME kernel-side pipeline the real cycle used — the pending filter,
    /// the live round-timing and momentum gates (with their near-miss and
    /// exemption trails), sizing, and order construction — mirroring the
    /// base walk's every-tick behaviour: the base re-collected these
    /// suggestions from unchanged inputs every tick and re-ran them into the
    /// same gates, so the replay reproduces its counters, logs and placement
    /// instants cycle for cycle without a Lua call. Deliberately NOT staged
    /// back into `last_candidates`: the staged set is a real evaluation's
    /// output, and every real evaluation (event wake, timing boundary, 2 s
    /// fallback) refreshes it. Returns None when the staged set is not in a
    /// stable phase (`candidates_stable == false`) — the caller must run a
    /// real evaluation instead, because the strategy's next collection would
    /// produce a post-transition output this cache cannot know.
    pub fn replay_candidates(&mut self, now_ms: i64) -> Option<Vec<crate::model::OrderRequest>> {
        if !self.candidates_stable {
            return None;
        }
        self.last_blocked.clear();
        self.last_exemptions.clear();
        // E3: same family resolution as the real evaluation — the table's
        // own duration on the legacy venue fold (single-family corpora read
        // the same numbers; the per-candidate refinements inside
        // `submit_candidate_orders` are each candidate's own family).
        let round = self.table_round_timing(now_ms);
        self.last_time_left_sec = round.time_left_sec;
        // The freshness rule is the one collection input that moves with the
        // clock alone (`now_ms - book.timestamp`): a book that has gone stale
        // since the staging evaluation drops its suggestion, exactly as the
        // base's re-collection at this tick would not have produced it.
        let round_tokens: HashSet<String> = self.round_token_ids().into_iter().collect();
        let candidates: Vec<TradeSignal> = self
            .last_candidates
            .iter()
            .filter(|sig| {
                round_tokens.contains(&sig.token_id)
                    && fresh_book(
                        &self.books,
                        &sig.token_id,
                        now_ms,
                        self.cfg.max_orderbook_stale_ms,
                    )
                    .is_some()
            })
            .cloned()
            .collect();
        Some(self.submit_candidate_orders(candidates, now_ms))
    }

    /// Issue #390 (replay driver wake): true when at least one staged
    /// candidate would survive the pending filter and therefore be re-run
    /// into the kernel-side gates on the next cycle — the state a skipped
    /// grid tick would NOT reproduce. Stability is deliberately NOT part of
    /// this verdict: an unstable set makes the next cycle a REAL evaluation
    /// (which re-collects and re-submits the same retry stream), a stable one
    /// makes it a replay — either way the base walk ran it on every tick, so
    /// the driver must not skip. Pending-covered candidates are excluded: the
    /// submission path drops them before any gate, counter or log line, so
    /// their quiet-tick re-emission is unobservable (the base proved this by
    /// running it every tick) and they must not pin the driver to the
    /// every-tick walk.
    pub fn has_standing_candidates(&self) -> bool {
        self.last_candidates.iter().any(|sig| {
            !self
                .pending_tokens
                .contains(&(sig.account_id.clone(), sig.token_id.clone()))
        })
    }
}
