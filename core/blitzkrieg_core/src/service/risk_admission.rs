//! The order-admission pipeline, the risk gate's IPC knobs and the kernel
//! heartbeat, moved verbatim from `service.rs` (pure code motion, no logic
//! edits): `place` with its full refusal ladder (cross-account, cross-venue,
//! duration-subscription, freeze), the maker/taker fill state machines, the
//! cancel/flatten verbs, the risk-limit writers (`risk.limits`,
//! `systemic.limits`, `exit.set`, `round_duration.set`), `system.reset_trading`,
//! the error/daily-budget emitters, and `tick` with the escalation and exit
//! sweeps.

use crate::ipc::schema::Event;
use crate::model::*;
use crate::ome::{FillDelta, SubmitParams};
use crate::risk::RiskConfig;
use crate::sim::Book;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

#[cfg(test)]
use super::config::VirtualAccountLimit;
use super::config::{EscalationTarget, FlattenTarget, InflightTaker, PlaceOutcome};

use super::Core;
use super::helpers::{escalation_delay_ms, exit_intent_key, is_close_intent};
use super::parse_direction;
use super::{EXIT_REPORT_BEAT_MS, marketable_exit_price};

impl Core {
    /// Freeze trading (kill switch) with a reason the panel can show. A
    /// restart or an explicit `risk.resume` clears it; the startup self-check
    /// re-probes either way. The freeze is logged at error level (#184) — a kill
    /// switch is the loudest thing this kernel does, and every path that can
    /// reach it (venue-refusal streak, failed self-check, failed sweep, audit
    /// halt) must be countable in a run log.
    ///
    /// It deliberately does NOT write the error slot: the slot keeps the ROOT
    /// CAUSE (the raw venue error that pushed a streak over the line), and
    /// `engine.stats.tradingFrozen.reason` already carries this message for the
    /// panel's freeze banner. Overwriting the slot here is what would make it
    /// show a paraphrase instead of the error an operator has to act on.
    pub(super) fn freeze_trading(&mut self, reason: String, now_ms: i64) {
        if self.risk.is_killed() {
            return; // already frozen — do not re-alarm on every rejection
        }
        let _ = now_ms;
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("trading frozen: {reason}"),
        });
        tracing::error!(reason = %reason, "trading frozen (kill switch)");
        self.emit_error_event(crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            reason,
        ));
    }

    /// Freeze ONE account (the account-scoped kill switch). Same 「拒开仓、
    /// 放平仓」 contract as the global kill and the trading switch: the
    /// submission gate refuses the account's new orders, the engine mirror
    /// drops its entry candidates, its exits keep flowing. The reason is
    /// shown by `account.list` (`accountFreezes`) and the doctor's accounts
    /// section. Idempotent: the first reason wins — a re-freeze while
    /// already frozen does not overwrite the cause an operator first saw.
    pub fn freeze_account(&mut self, account: &str, reason: String) {
        if self.account_freezes.contains_key(account) {
            return;
        }
        self.account_freezes
            .insert(account.to_string(), reason.clone());
        self.engine_set_account_frozen(account, true);
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("account {account} frozen: {reason}"),
        });
        tracing::error!(account = %account, reason = %reason, "account frozen (account-scoped kill switch)");
    }

    /// The AUTOMATIC freeze route: [`Self::freeze_account`] plus a timestamp
    /// in `auto_frozen` — but only when the freeze actually landed. The
    /// freeze is idempotent (first reason wins), so a re-freeze of an
    /// already-frozen account keeps the ORIGINAL stamp: the clock an
    /// auto-lift debounces against is the moment the operator first lost
    /// the account, not the latest symptom.
    ///
    /// 手动冻结（`account.freeze` 直达 `freeze_account`）从不登记 —— 操作员
    /// 的裁决只能由操作员推翻，健康探针不得越权。
    pub(super) fn freeze_account_auto(&mut self, account: &str, reason: String, now_ms: i64) {
        let fresh = !self.account_freezes.contains_key(account);
        self.freeze_account(account, reason);
        if fresh {
            self.auto_frozen.insert(account.to_string(), now_ms);
        }
    }

    /// The OPERATOR freeze route: [`Self::freeze_account`] plus dropping any
    /// `auto_frozen` stamp first.
    ///
    /// `freeze_account` is idempotent, so freezing an account the kernel had
    /// ALREADY auto-frozen used to leave the auto stamp in place — the next
    /// healthy-venue sweep (`auto_lift_if_venue_healthy`) then resumed the
    /// account and the operator's kill switch was silently lifted. Clearing the
    /// ledger makes the operator verdict outrank the self-heal by construction.
    pub fn freeze_account_manual(&mut self, account: &str, reason: String) {
        self.auto_frozen.remove(account);
        self.freeze_account(account, reason);
    }

    /// 冻结自愈的解冻半边：only an AUTO-frozen account may self-lift, and
    /// only on positive evidence the venue recovered (`trigger` names it).
    /// A manual freeze is never touched — `auto_frozen` is the ledger that
    /// separates kernel circumstantial freezes from operator verdicts.
    pub(super) fn auto_lift_if_venue_healthy(&mut self, account: &str, trigger: &str, now_ms: i64) {
        let Some(frozen_at) = self.auto_frozen.get(account).copied() else {
            return;
        };
        self.resume_account(account);
        self.auto_frozen.remove(account);
        let minutes = (now_ms - frozen_at).max(0) / 60_000;
        tracing::warn!(
            account = %account,
            frozen_duration_ms = now_ms - frozen_at,
            trigger = %trigger,
            "auto-frozen account auto-lifted — venue proved healthy"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!(
                "自动冻结已解除（{trigger}）：账户 {account} 冻结 {minutes} 分钟后场地恢复"
            ),
        });
    }

    /// Lift one account's freeze (`account.freeze` with `frozen: false`, the
    /// reset path, or a future operator verb). Unknown account = no-op: the
    /// freeze book only ever contains accounts the kernel knows.
    pub fn resume_account(&mut self, account: &str) {
        if self.account_freezes.remove(account).is_some() {
            self.engine_set_account_frozen(account, false);
            tracing::info!(account = %account, "account freeze lifted");
        }
        // 手动解冻也要摘掉自愈时间戳：freeze book 与 auto ledger 对同一
        // 账户要么都有意、要么都干净，半悬空状态只会让下一次 auto-lift
        // 读到一个已不存在的冻结。
        self.auto_frozen.remove(account);
    }

    /// Why an account is frozen, when it is.
    pub fn account_freeze_reason(&self, account: &str) -> Option<&str> {
        self.account_freezes.get(account).map(String::as_str)
    }

    /// The whole freeze book, for the stats readout.
    pub fn account_freezes(&self) -> &HashMap<String, String> {
        &self.account_freezes
    }

    /// Is this account's order admitted under the account-scoped kill
    /// switch? CLOSING intents always pass (the #174 escape hatch is per
    /// account here, exactly as it is globally). Called from the submission
    /// gate right beside Gate 2.
    pub(super) fn account_freeze_permits(&self, req: &OrderRequest) -> CoreResult<()> {
        if let Some(reason) = self.account_freezes.get(req.account_id.as_str())
            && !crate::risk::is_close_intent(&req.internal_key)
        {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                format!(
                    "account `{}` is frozen (account-scoped kill switch): {reason} — \
                     new orders refused, exits keep flowing",
                    req.account_id
                ),
            ));
        }
        Ok(())
    }

    /// Push one account's freeze fact into the engine mirror. `None` = the
    /// engine is not installed (backtest/in-process shapes) — nothing to
    /// mirror.
    pub(super) fn engine_set_account_frozen(&mut self, account: &str, frozen: bool) {
        if let Some(e) = self.engine.as_mut() {
            e.set_account_frozen(account, frozen);
        }
    }

    pub(super) fn emit(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }

    pub(super) fn emit_order(&self, id: &str) {
        if let Some(o) = self.ome.get(id) {
            // Durable persistence: every status change flows through here (submit /
            // confirm / reject / bind / cancel), so the order log always reflects
            // the latest state and a restart can rebuild it (orphan guard).
            self.persist_order(id);
            self.emit(Event::OrderUpdate { order: o.clone() });
        }
    }

    pub(super) fn emit_fill(&self, d: FillDelta) {
        if let Some(o) = self.ome.get(&d.order_id) {
            self.persist_order(&d.order_id);
            self.emit(Event::Fill {
                delta: d.into(),
                order: o.clone(),
            });
        }
    }

    pub(super) fn new_order_id(&mut self) -> String {
        // "live" is reserved for orders that can actually reach a venue. A
        // read-only order is not live — stamping it `live_` would put a
        // misleading label on the one field an operator reads to decide what is
        // real, so it is named for what it is.
        let prefix = match self.config.mode {
            Mode::Live => "live",
            Mode::ReadOnly => "readonly",
            Mode::Dry => "dry",
        };
        let id = format!("{prefix}_{}", self.next_id);
        self.next_id += 1;
        id
    }

    // ── Risk ───────────────────────────────────────────────────────────────
    pub fn kill(&mut self, reason: String) {
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::KillSwitchActive,
            message: reason,
        });
    }

    /// Flush pending near-miss records to disk (call on shutdown).
    pub fn flush_near_misses(&mut self) {
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.flush_near_misses())
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss flush failed");
            }
        }
    }

    pub fn resume(&mut self) {
        self.risk.resume();
    }

    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// Hot-swap the live risk gate configuration without a restart (test and
    /// runtime-config path; `place()` reads it on every check).
    pub fn risk_config_mut(&mut self) -> &mut RiskConfig {
        self.risk.config_mut()
    }

    /// Test-only: re-arm the virtual-account book after construction (the
    /// production surface is `CoreConfig::virtual_limits` at boot; tests
    /// build the core first and opt a strategy in afterwards).
    #[cfg(test)]
    pub(crate) fn set_virtual_limits_for_test(
        &mut self,
        limits: HashMap<String, VirtualAccountLimit>,
    ) {
        self.virtual_accounts = crate::risk::virtual_account::VirtualAccounts::new(&limits);
    }

    /// The live risk gate config — the read side of
    /// [`Self::risk_config_mut`]: a `risk.setLimits`/`risk.setSystemic` write
    /// is visible here immediately, which is what keeps the `risk.limits`
    /// readout honest after a hot edit.
    pub fn risk_config(&self) -> &RiskConfig {
        self.risk.config()
    }

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
    pub(super) fn place_escalated(
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
    pub(super) fn place_inner(
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
            self.fast_orders_dirty = true;
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
    pub(super) fn refuse_cross_account_close(&self, req: &OrderRequest) -> CoreResult<()> {
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
    pub(super) fn refuse_cross_venue_order(&self, req: &OrderRequest) -> CoreResult<()> {
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
    pub(super) fn account_duration_subscriptions(
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
    pub(super) fn refuse_unsubscribed_duration(&self, req: &OrderRequest) -> CoreResult<()> {
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
    pub(super) fn duration_of_token_for_admission(&self, token_id: &str) -> i64 {
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
    pub(super) fn stage_duration_subscriptions_for_test(&mut self, subs: HashSet<(String, i64)>) {
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

    pub(super) fn place_gated(
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
    pub(super) fn marketable_walk(
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
    pub(super) fn books_round_slot(&self, token: &str) -> Option<i64> {
        self.engine.as_ref().and_then(|e| e.round_slot_of(token))
    }

    pub(super) fn books_asset(&self, token: &str) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|e| e.asset_of(token))
            .map(|a| a.to_string())
    }

    pub(super) fn books_direction(&self, token: &str) -> Option<String> {
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
    pub(super) fn taker_refusal(&self, order: &TrackedOrder, book: &Book) -> CoreError {
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
    pub(super) fn place_after_submit(
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
    pub(super) fn authoritative_fill(
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
    pub(super) fn drain_inflight_takers(&mut self, now_ms: i64) {
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
    pub(super) fn try_maker_fill(&mut self, id: &str, now_ms: i64) {
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

    pub fn cancel(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id))?;
        let was_live = order.status.is_live();
        if order.side == Side::Buy {
            // E28: the reservation is released in the ORDER'S OWN book — a
            // cancel must never refund another account's cash.
            if let Ok(l) = self.accounts.get_mut(&order.account_id) {
                l.release(id);
            }
            // E-VA: the same precision for the strategy's VIRTUAL pot — the
            // cancelled order's whole remaining hold leaves the book.
            self.virtual_release_order(&order);
        }
        self.ome.mark_terminal(id, OrderStatus::Cancelled, now_ms)?;
        // RFC 568 阶段一：死亡事实（撤单）喂配对簿。
        self.pair_on_order_event(&order.internal_key, Decimal::ZERO, true);
        // The token's repeat-suppression ends with the order: a cancelled
        // entry leaves no resting order behind, so a later cycle may re-attempt
        // (the strategy's own latch still bounds it to one attempt per book).
        if was_live && let Some(engine) = self.engine.as_mut() {
            engine.release_pending_token_for_strategy(
                order.account_id.as_str(),
                &order.strategy,
                &order.token_id,
            );
        }
        // Replay fast path: a live order just left the book — the escalation
        // sweep and the audit input changed.
        if was_live && self.replay_sched() {
            self.fast_orders_dirty = true;
        }
        // The venue may still hold the order resting: hand its id to the live
        // bridge for a real cancel. Nothing to cancel when the venue never
        // acknowledged the order (no id — the venue never heard of it).
        if was_live && let Some(vid) = order.venue_order_id {
            let venue = self.execution_venue_of(order.account_id.as_str(), &order.token_id);
            self.queue_venue_cancel(venue, vid);
        }
        self.emit_order(id);
        Ok(())
    }

    /// Enqueue one venue cancel (deduped by id, bounded so a pathological loop
    /// cannot grow it without bound). The live bridge drains it every tick.
    /// `venue` labels the entry for the per-venue executor drain (`""` =
    /// untagged — routes to the active plugin's drain, the single-plugin
    /// behaviour).
    pub(super) fn queue_venue_cancel(&mut self, venue: String, venue_order_id: String) {
        if !self
            .pending_venue_cancels
            .iter()
            .any(|(_, id)| id == &venue_order_id)
        {
            self.pending_venue_cancels.push((venue, venue_order_id));
            if self.pending_venue_cancels.len() > 256 {
                self.pending_venue_cancels.remove(0);
            }
        }
    }

    /// Drain the venue-cancel queue: once. The bridge must forward each id as
    /// a real `DELETE /order` — see [`Self::cancel`].
    pub fn take_pending_venue_cancels(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_venue_cancels)
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    }

    /// The multi-plugin drain: every cancel id whose order's round market
    /// names `venue`, plus the UNTAGGED ids when `venue` is the host's active
    /// plugin (the single-plugin fallback the orders drain uses). Take-once
    /// per entry: a sibling executor never sees an id this drain returned.
    pub fn take_pending_venue_cancels_for_venue(
        &mut self,
        venue: &str,
        untagged_fallback: bool,
    ) -> Vec<String> {
        let mut taken = Vec::new();
        let mut rest = Vec::new();
        for (v, id) in std::mem::take(&mut self.pending_venue_cancels) {
            if v == venue || (untagged_fallback && v.is_empty()) {
                taken.push(id);
            } else {
                rest.push((v, id));
            }
        }
        self.pending_venue_cancels = rest;
        taken
    }

    /// E31-c: retire the RESTING remainder of a partially filled order while
    /// keeping the already-filled part in the position book. The strategy
    /// decides WHEN; the kernel guarantees the venue-side cancel rides along.
    pub fn cancel_remaining(&mut self, order_id: &str, now_ms: i64) -> CoreResult<()> {
        self.cancel(order_id, now_ms)
    }

    /// E31-c: flatten the position behind an order — sell every share the
    /// fills have accrued so far, at that position's latest valuation.
    pub fn close_filled(&mut self, order_id: &str, now_ms: i64) -> CoreResult<usize> {
        let token = self
            .ome
            .get(order_id)
            .map(|o| o.token_id.clone())
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, order_id.to_string()))?;
        let position = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.token_id == token)
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorCode::UnknownOrder,
                    format!("no open position behind order {order_id}"),
                )
            })?;
        self.flatten(Some(&position), now_ms)
    }

    pub fn cancel_all(&mut self, token_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| token_id.map(|t| o.token_id == t).unwrap_or(true))
            .map(|o| o.order_id.clone())
            .collect();
        for id in &ids {
            self.cancel(id, now_ms)?;
        }
        Ok(ids.len())
    }

    /// Force-close positions: a specific id, or all when id is None. Returns how
    /// many were closed. Exits are SELL orders; in dry mode they cross at the
    /// latest book bid, in live mode the venue bridge submits them.
    pub fn flatten(&mut self, position_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let targets: Vec<FlattenTarget> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| position_id.map(|id| p.id == id).unwrap_or(true))
            .map(|p| {
                (
                    p.id.clone(),
                    p.token_id.clone(),
                    p.condition_id.clone(),
                    p.shares,
                    p.strategy.clone(),
                    p.asset.clone(),
                    p.direction.as_str().to_string(),
                    p.current_price,
                    // E28 (§9.2): the manual close spends from the position's
                    // own account.
                    p.account_id.clone(),
                )
            })
            .collect();
        let mut closed = 0usize;
        for (id, token, condition, shares, strategy, asset, direction, _current, account_id) in
            targets
        {
            // Already have a live sell for this token? skip.
            if self
                .ome
                .live_for(&token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live())
            {
                continue;
            }
            // Price the exit off the CURRENT book, not the stale valuation:
            // `current_price` is a mid ≈ what the shares are worth, not what
            // a resting bid is offering, so an FOK there never crosses live.
            // The walk names the worst bid level that covers the size. With
            // no bids at all nothing can fill — the request falls back to the
            // grid floor and the settle/venue rejects it honestly.
            let price = self
                .marketable_walk(Side::Sell, &token, shares, Decimal::new(1, 2), now_ms)
                .map(|(_, worst)| worst)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(Decimal::new(1, 2));
            self.note_exit_reason(&token, ExitReason::Manual);
            let order = OrderRequest {
                token_id: token,
                condition_id: condition,
                side: Side::Sell,
                mode: FillPolicy::Taker,
                price,
                size: shares,
                internal_key: format!("flatten:{id}"),
                strategy,
                asset,
                direction,
                round_slot: 0,
                // E28 (§9.2): the closing sell spends from the position's own
                // account — a cross-account flatten must never touch another
                // book.
                account_id,
            };
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            } else {
                closed += 1;
            }
        }
        Ok(closed)
    }

    // ── Maintenance: pending-fill retry + maker→taker escalation + exits ────
    pub fn tick(&mut self, now_ms: i64) -> CoreResult<()> {
        // #351: advance in-flight taker legs — the tick path covers rounds
        // where the clock moves without data events (the replay tail keeps
        // reporting legs after the last event). A no-op when no leg waits.
        self.drain_inflight_takers(now_ms);
        // Flush the market-data archive at most once a second, so a reader of the
        // live file stays close behind without paying a write syscall every tick
        // (P-1.3).
        if let Some(a) = self.event_archive.as_mut() {
            a.flush_if_due(now_ms, 1000);
        }
        // Issue #669: the archive's stop (disk floor / lock / io / session cap)
        // is otherwise a single journal warn line — the external operator who
        // reported it found the silent corpus loss FIVE DAYS later with a manual
        // `df`. Fold the live recording state through the last-seen slot and
        // say it OUT LOUD on the `true → false` edge, exactly once: the risk
        // alert carries the stop reason and what is being lost (backtest
        // corpus). Staying stopped is silent; the panel banner reads
        // `stats.archive` continuously. `false → true` cannot happen (a stopped
        // archive never restarts) but resets the slot defensively.
        if let Some(a) = self.event_archive.as_ref() {
            let recording = a.status().recording;
            if self.archive_recording_last && !recording {
                let message = "事件归档停录——回测语料不再记录；请清理磁盘或调整归档上限后重启会话"
                    .to_string();
                eprintln!("core: {message}");
                tracing::error!("{}", message);
                self.note_error(
                    &CoreError::new(CoreErrorCode::Internal, message.clone()),
                    now_ms,
                );
                self.emit(Event::RiskAlert {
                    code: CoreErrorCode::Internal,
                    message,
                });
            }
            self.archive_recording_last = recording;
        }

        // E29 (§12.2): the still-growing bars push at most 1/s per
        // `(symbol, interval)` — the "正在长" preview a chart draws translucent.
        // CLOSED bars are pushed by `engine_on_data` the instant they close and
        // are NEVER subject to this throttle (a swallowed close would leave the
        // chart missing that bar forever). The throttle is per bucket on the
        // CORE's clock, shared by every subscribed session.
        // Replay gate (issue #409 P0, 39.5× measured): the pass is skipped
        // wholesale under `replay_sched()`. Its events are live-panel
        // decoration and NOTHING in a replay consumes them — the backtest
        // drain matches every event it cares about by name and drops
        // `KlineUpdate` in `_ => {}`; the `next_replay_wake_ms` probe has
        // never armed a wake for this pass (its only throttle state is the
        // local 1/s fold); and `engine_on_data` pushes CLOSED bars on its
        // own path, untouched by this gate. The per-bucket 1/s fold, the
        // "one bar one push" contract and the close-vs-preview split are all
        // live semantics; dropping them in replay changes no byte of the
        // report — the A/B in docs/perf/backtest-perf-plan.md measured the
        // output bit-identical with the pass off.
        if !self.replay_sched()
            && let Some(engine) = self.engine.as_ref()
        {
            for bar in engine.kline_current_bars() {
                let key = (bar.symbol.clone(), bar.interval);
                let due = self
                    .kline_preview_last_ms
                    .get(&key)
                    .is_none_or(|last| now_ms - *last >= 1_000);
                if due {
                    self.kline_preview_last_ms.insert(key, now_ms);
                    self.emit(Event::KlineUpdate { kline: bar });
                }
            }
        }

        // Retry buffered fills (orders registered since the event arrived).
        let pending = self.ome.drain_pending(now_ms)?;
        for outcome in pending {
            self.report_fill_outcome(outcome, now_ms);
        }

        // Settlement (issue #175): a position past its market's expiry has to
        // become cash, and only the news of the market's resolution can do it.
        // Runs before the audit so the money a settlement moves is accounted in
        // the same round it happens (the ordering is free: `drive_settlement` is
        // a no-op in live mode, where resolutions arrive from the venue).
        // Replay fast path: the whole pass is a pure function of the position
        // set and the two deadline clocks (query re-query, redemption retry) —
        // skipped until the position set changes, the earliest expiry passes,
        // or a deadline arrives. A skipped tick would have dispatched nothing
        // and stamped nothing, so every dispatch keeps its exact stamp.
        // A pending merge claim joins the deadlines: its dispatch clock must
        // keep ticking even when nothing else in the pass moves.
        let settlement_pass = !self.replay_sched()
            || self.fast_positions_dirty
            || now_ms >= self.fast_next_expiry_ms
            || now_ms >= self.settlement.next_query_deadline_ms()
            || now_ms >= self.settlement.next_redemption_ms()
            || now_ms >= self.settlement.next_merge_ms();
        if settlement_pass {
            self.drive_settlement(now_ms);
            // A market we are watching whose verdict never arrives strands the
            // position: say so once per episode (the panel carries
            // `blindSinceMs` continuously, this is the alert). The blind edge
            // only changes when queries/answers move, so riding the same gate
            // delays the alert at most to the next deadline tick.
            self.alert_if_settlement_blind(now_ms);
            // #684: same gate, one step further — a market the venue will
            // STRUCTURALLY never answer (expired rounds drop out of the updown
            // list) cannot be settled by waiting. The sweep only fires when
            // `unanswered_since` names it, so a healthy book pays one
            // hash-set build per settlement pass and nothing else.
            self.run_expiry_fallback_sweep(now_ms);
        }

        // Accounting audit (issue #189): the kernel's own periodic three-way money
        // check, so drift is caught (and blocks new entries) even when nothing
        // external is polling. Cheap: sums over live orders and open positions.
        if self.config.audit_interval_sec > 0 {
            let due = self.audit.last_at_ms == 0
                || now_ms - self.audit.last_at_ms >= self.config.audit_interval_sec * 1000;
            if due {
                self.run_accounting_audit(now_ms);
            }
        }

        // Resume entries when a per-strategy breaker cooldown elapses (KI-10):
        // each strategy resumes independently and is reported by name.
        for strategy in self.breaker.maybe_resume_all(now_ms) {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "breaker cooldown elapsed — resuming new entries for strategy {strategy}"
                ),
            });
        }

        // 探针 10s 汇总（任务二 1-3，受控时钟）：tick 断流 / spot 失效的周期
        // 判定只由调用方传入的 `now_ms` 驱动——生产是墙钟，回放是虚拟时钟，
        // 同一函数、同一节奏，无墙钟窗口（memory: wallclock-test-flake 处方）。
        // 只观测：warn + 计数，不拦截任何东西。
        if self.probe_summary_due(now_ms) {
            self.probe_run_summary(now_ms);
        } else {
            // 非汇总 tick：汇总读过的窗口自此重新计数。
            self.probe_ticks.clear_window_if_consumed();
        }

        // Persist finalized near-miss records (blocked signals + their paths).
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.take_near_misses(now_ms))
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss persist failed");
            }
        }

        // #173: roll the daily-loss budget at the UTC day boundary BEFORE the
        // exit checks, so a close that lands on the new day is counted in it
        // — and report the state that gates entries: the roll itself (audit
        // trail), a restored/just-tripped breaker, and #177's withheld stops.
        self.roll_daily_budget(now_ms);
        self.emit_daily_trip();

        // Evaluate exits for every open position and place a closing SELL.
        self.run_exit_checks(now_ms)?;
        // Replay fast path: the dirty-token set is consumed by this cycle's
        // valuation — clear it here (not inside run_exit_checks, whose early
        // returns would skip the clear) so it never becomes a permanent
        // over-approximation. Inserts landing between here and the next
        // tick's exit pass are events that arrive after this point by
        // construction, so nothing is lost. #390 (nofast): the same set wakes
        // the deadline-scheduled ladder, so it is cleared here on the nofast
        // path too — after `run_exit_checks` had its look.
        self.fast_dirty_tokens.clear();
        if self.replay_sched() && !self.config.backtest_fast {
            // #390 (merged maintenance): re-arm the single maintenance
            // deadline — the merged pass's own 1 s beat (report stamps are
            // quantised here in the base) plus the earliest position expiry
            // (time-to-expiry rules). Between these, a quiet grid re-observes
            // the previous run's inputs; dirty tokens wake the pass early on
            // their own.
            self.fast_next_exit_deadline_ms = self
                .positions
                .next_maintenance_deadline_ms(now_ms, EXIT_REPORT_BEAT_MS)
                .unwrap_or(i64::MAX);
        }

        // #177: a protective stop the wick guard withheld is an event, not a
        // silent no-op — review must be able to see "should have triggered".
        self.emit_suppressed_stops();

        // #457: an order resting on an EXPIRED round can never fill — close it
        // BEFORE the escalation sweep, so a dead round is never chased.
        // Live-clock only (see the sweep's own replay note).
        self.run_expiry_sweep(now_ms);
        // E-VA companion: a pair leg whose partner is gone (filled one way,
        // cancelled the other) past the landing grace window is a naked leg —
        // alert once, flatten it. Governed atomic scopes only; live path
        // only (the nofast replay loop keeps its own shape).
        self.run_pair_orphan_sweep(now_ms);
        // Escalate due maker_then_taker orders: cancel maker, cross as taker.
        // Replay fast path: the sweep over every live order runs only when
        // the order set changed, or when the earliest armed escalation
        // deadline has arrived — between those, no `escalate_at_ms` can be
        // due (arming never moves a deadline earlier than the sweep that
        // would notice it recomputes the floor).
        if !self.replay_sched() || self.fast_orders_dirty || now_ms >= self.fast_next_escalation_ms
        {
            self.run_escalation_sweep(now_ms)?;
            if self.replay_sched() {
                self.fast_orders_dirty = false;
                self.fast_next_escalation_ms = self.ome.next_escalation_ms();
            }
        }
        Ok(())
    }

    /// #461 Fix 2/4: the COMPLETE risk verdict an escalated taker leg must
    /// pass before the maker is touched. Both escalation paths (the timer
    /// sweep and the post-only WouldCross reaction) share this ONE function —
    /// there is no third vocabulary.
    ///
    /// Layers, in order:
    ///   1. `check_with_equity` with the REAL account balance — the bare
    ///      `risk.check` the paths used before passed equity 0, which
    ///      silently disabled the #202 equity-relative cap at the exact
    ///      moment the order chases the worst price on the book;
    ///   2. `judge_entry` (the nine systemic limits) when any bound is armed
    ///      — the same verdict the arbitration pipeline hands strategy
    ///      entries, so chasing cannot outrun a cap the entry obeyed;
    ///   3. the PAIR-COST hard cap: for an entry whose condition already has
    ///      a counterpart position (a maker leg that filled or is filling),
    ///      `pair_paid + chase_cost <= $1.00` — a binary pair redeems exactly
    ///      $1, so a higher total is a GUARANTEED loss, not a risk. The
    ///      "entry gate forbids" comment in `PositionManager::merge_condition`
    ///      asserted this invariant; this is the code that makes it true.
    ///
    /// Returns Err with the reason; the caller keeps the maker resting (timer
    /// path) or books the rejection (WouldCross path) — the escalation intent
    /// survives either way via the normal re-arm/backoff machinery.
    pub(super) fn escalation_guard(
        &self,
        req: &OrderRequest,
        chase_price: Decimal,
    ) -> Result<(), String> {
        // 1. The ordinary submission gate, with the account's real equity.
        let equity = self
            .accounts
            .ledger_of(&req.account_id)
            .map(|l| l.balance())
            .unwrap_or(Decimal::ZERO);
        if let Err(e) = self.risk.check_with_equity(req, equity) {
            return Err(e.message);
        }
        // 2. The systemic nine. Armed bounds judge this entry exactly as the
        //    pipeline would judge a fresh strategy entry (the aggregates are
        //    built the same way — positions only, the incoming notional on top).
        let limits = self.risk.systemic_snapshot();
        if limits.any_armed() {
            let now_ms = blitzkrieg_market_api::net::now_ms();
            let facts = crate::risk::limits::SystemicFacts {
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
                    .sum(),
                same_asset_exposure_usd: self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.asset == req.asset)
                    .map(|p| p.cost_usd)
                    .sum(),
                day_realized_usd: self.positions.daily_pnl_total(),
            };
            // Same doctrine as the pipeline gate: a holder's bound stop is
            // the wide containment width the walker will actually enforce.
            let holder = self
                .engine
                .as_ref()
                .is_some_and(|e| e.strategy_holds_to_settlement(&req.strategy));
            let bound_stop_pct = if holder {
                crate::exit_policy::holder_effective_stop_pct(0, &self.config.positions.exit)
            } else {
                crate::exit_policy::effective_stop_pct(
                    self.config.positions.exit.stop_loss_pct,
                    // Time left is unknown on this path; 0 = the bound stop at
                    // its tightest (no time-adjustment relief), the
                    // conservative read.
                    0,
                    &self.config.positions.exit,
                )
            };
            if let crate::risk::limits::EntryJudgment::Reject { detail, .. } =
                crate::risk::limits::judge_entry(
                    limits,
                    &facts,
                    chase_price,
                    req.size,
                    bound_stop_pct,
                )
            {
                return Err(detail);
            }
        }
        // 3. The pair-cost cap. Only entries (BUY) chase; a condition with no
        //    counterpart position has nothing to sum against (a fresh pair is
        //    the strategy's own <1.00 decision). Per-share: one UP share plus
        //    one DOWN share redeems exactly $1.00. 跨账户双腿：分母只看本
        //    账户的对手腿（同一账本内赎回 $1）。
        if req.side == Side::Buy
            && let Some(paid) = self.counterpart_pair_basis(
                &req.condition_id,
                &req.asset,
                &parse_direction(&req.direction),
                &req.account_id,
            )
        {
            let total = paid + chase_price;
            if total > Decimal::ONE {
                return Err(format!(
                    "pair-cost cap: counterpart leg basis {paid}/share + chasing at \
                     {chase_price} = {total} > $1.00 a pair redeems — the merge would \
                     lock in a loss"
                ));
            }
        }
        Ok(())
    }

    /// Escalate due maker_then_taker orders: cancel the resting maker, cross
    /// the remainder as a taker. Extracted from `tick` so the replay fast
    /// path can gate the sweep (order-set dirty OR earliest deadline due)
    /// without duplicating the submission rules.
    pub(super) fn run_escalation_sweep(&mut self, now_ms: i64) -> CoreResult<()> {
        let due: Vec<EscalationTarget> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.escalate_at_ms.map(|t| t <= now_ms).unwrap_or(false))
            .map(|o| {
                (
                    o.order_id.clone(),
                    o.side,
                    o.size - o.filled_size,
                    o.token_id.clone(),
                    o.condition_id.clone(),
                    o.strategy.clone(),
                    o.asset.clone(),
                    o.direction.clone(),
                    o.round_slot,
                    // The maker's own timeout, rebuilt from when it was
                    // armed: a re-armed clock waits the same interval again.
                    o.escalate_at_ms.unwrap_or(now_ms) - o.submitted_at_ms,
                    // E28 (§9.2): the taker leg spends from the maker's account.
                    o.account_id.clone(),
                )
            })
            .collect();
        // #774 test seam: a fill that lands exactly between the snapshot just
        // taken and the cancel below. Production has no mutation point there
        // (the sweep runs under the core lock), so the regression test stages
        // the fill and it is applied — through the ordinary fill path — here.
        #[cfg(test)]
        if let Some(fill) = self.test_escalation_snapshot_fill.take() {
            self.ingest_fill(fill, now_ms)?;
        }
        for (
            id,
            side,
            remaining,
            token,
            condition,
            strategy,
            asset,
            direction,
            slot,
            timeout,
            account_id,
        ) in due
        {
            if remaining <= Decimal::ZERO {
                continue;
            }
            // Reprice the taker leg from the CURRENT book before touching the
            // maker: the resting limit is a passive price — a live FOK stated
            // there would be killed, so escalating to it just moves the
            // failure. If the book cannot cover the remaining size even at
            // the extreme of the grid, leave the maker resting and re-arm the
            // clock: no venue traffic, the entry intent stays alive.
            let cap = if side == Side::Buy {
                Decimal::new(99, 2)
            } else {
                Decimal::new(1, 2)
            };
            let Some((_, worst)) = self.marketable_walk(side, &token, remaining, cap, now_ms)
            else {
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            };
            // issue 568 阶段一：定时 escalation 的追单是同一入腿事实链的
            // 续段 —— 成交/死亡事实寻址回声明腿（原腿 cancel 的死亡事实已
            // 落账；续段 key 在追单 place 前登记，其 Taker 成交照旧记账）。
            self.pair_book.bind_continuation(
                &strategy,
                account_id.as_str(),
                &token,
                &format!("{id}:escalated"),
            );
            let mut req = OrderRequest {
                token_id: token,
                condition_id: condition,
                side,
                mode: FillPolicy::Taker,
                price: worst,
                size: remaining,
                internal_key: format!("{id}:escalated"),
                strategy,
                asset,
                direction,
                round_slot: slot,
                // E28 (§9.2): the escalated leg spends from the maker's own
                // account.
                account_id: account_id.clone(),
            };
            // The maker is about to be cancelled to make room for this leg, so
            // the FULL risk verdict gets the first word — the same verdict the
            // submission path will apply a moment later (#461: real equity +
            // systemic limits + the pair-cost cap, one function for both
            // escalation paths). `marketable_walk` only asks whether the
            // book HAS depth at the grid's extreme; it does not ask whether
            // this core may afford it. With a per-order notional cap, the
            // affordable price is `cap / size`, and a book that only offers
            // depth above it (a one-sided 0.99 ask is the everyday case) makes
            // a naive escalation cancel the maker and THEN get refused: the
            // entry is destroyed rather than deferred, and the propagated
            // error aborts the whole tick. Skipping instead keeps the resting
            // maker alive and re-arms the clock — the same semantics as the
            // no-depth branch above — while saying so once per order.
            if let Err(reason) = self.escalation_guard(&req, worst) {
                if self.escalation_skips.insert(id.clone()) {
                    tracing::info!(
                        order = %id,
                        strategy = %req.strategy,
                        price = %worst,
                        size = %remaining,
                        reason = %reason,
                        "escalation skipped: the chase is refused by the full risk guard; maker leg kept resting"
                    );
                }
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            }
            self.escalation_skips.remove(&id);
            self.cancel(&id, now_ms)?;
            // issue #774: the snapshot above predates the cancel, and cancelling
            // is a venue round-trip. A fill the venue booked in that window (the
            // event path has already projected it into the OME) moved
            // `filled_size`, so the true remainder is smaller than the one the
            // request above was built with — placing it would overshoot the
            // intended size (total filled > size). Re-read the order AFTER the
            // retire and chase only what is still unfilled.
            //
            // If the late fill consumed the order there is nothing left to
            // chase: place NO replacement and let the escalation end fully
            // filled.
            //
            // What remains uncertain: a fill still in flight at the venue when
            // this read runs (one the OME has not seen yet) is NOT covered — the
            // replacement can still overshoot by that in-flight size. Closing
            // that window needs the venue's cancel acknowledgement and a final
            // fill read after it; that protocol is out of scope here.
            let fresh = self.ome.remaining(&id);
            if fresh <= Decimal::ZERO {
                tracing::info!(
                    order = %id,
                    strategy = %req.strategy,
                    stale_size = %req.size,
                    "escalation finished as fully filled: the maker filled between the snapshot and the cancel; no replacement placed"
                );
                continue;
            }
            req.size = fresh;
            // Past the gate, a placement can still fail (a reservation, a
            // venue refusal). The maker is already gone, so that entry is
            // lost — but ONE dead escalation must not abort the maintenance
            // pass: the remaining due orders still have their turn.
            if let Err(e) = self.place_escalated(req, now_ms) {
                tracing::warn!(
                    order = %id,
                    error = %e,
                    "the escalated leg was refused after the maker was cancelled"
                );
            }
        }
        // Forget the skip marks of orders that are no longer live: the set is
        // keyed by order id, and a fresh id is minted for every order.
        let ome = &self.ome;
        self.escalation_skips
            .retain(|id| ome.get(id).is_some_and(|o| o.status.is_live()));
        Ok(())
    }

    /// Check exits and submit a closing SELL for each triggered position.
    ///
    /// Two sources converge on ONE submission loop so both are subject to the
    /// same rules (live-sell dedup, `sell_shares`, risk/ledger/sign in `place`):
    ///  - the kernel's automated exit policy (`check_exits`), suppressed by
    ///    `auto_exits_enabled = false`;
    ///  - explicit strategy close intents (`StrategySignal`), which like a
    ///    manual flatten are honoured even when automated exits are disabled,
    ///    but still cannot bypass the kill switch/risk/dedup/position checks.
    pub(super) fn run_exit_checks(&mut self, now_ms: i64) -> CoreResult<()> {
        // Consume this cycle's intents up front: an intent on a token with no
        // open position is dropped, never left to fire on a later position.
        let intents = std::mem::take(&mut self.strategy_exits);
        if self.positions.open_positions().is_empty() {
            return Ok(());
        }

        // MERGE interception (task 2.2): a strategy close intent tagged "merge"
        // is a pair-COLLECTION request, not a market exit. Complete UP+DOWN
        // pairs of the intent's condition are burned into their on-chain
        // collateral ($1 per pair) immediately — no order is placed, no ladder
        // prices it, the on-chain merge IS the fill, exactly like settlement's
        // redemption. Unpaired remainders stay open for settlement; an intent
        // with no complete pair behind it is dropped like every intent.
        // Runs BEFORE any book borrow: a merge is priced by construction
        // ($0.50 per leg), never off a quote.
        // Read the live books through a shared immutable borrow instead of
        // cloning the whole map per tick: the valuate/check_exits closure only
        // needs the few tokens behind open positions, not every mirrored book.
        // (Scoped before the merge interception: the closure must drop before
        // the merge's `&mut self` calls.)
        // Issue #390 (replay scheduling, marks only): the base's every-tick
        // valuate advances `current_price`/`prev_price`/`last_book_ts` before
        // its ladder runs, and consumers priced off the mark between the two
        // passes (strategy-signal exits below) see the fresh quote. The merged
        // replay pass reproduces that ordering with a mark-only refresh —
        // dirty tokens on a skip tick (only their book changed), every
        // position on a run tick — and leaves the counter ratchet to the
        // interval backfill inside the ladder.
        if !self.config.backtest_fast {
            let marks_all = !self.config.replay_scheduling
                || !self.fast_dirty_tokens.is_empty()
                || now_ms >= self.fast_next_exit_deadline_ms;
            // Issue #390: the scalar projection (BookView) instead of the
            // full snapshot — the marks refresh reads best_bid/mid_price/
            // timestamp only, so the two owned level-vector clones and the
            // token String per position per tick are gone. `from_book` does
            // the five Decimal reductions ONCE per mirrored book (the
            // closure runs once per position; identical inputs, Copy output).
            // #462: the timestamp passed is the book's own RECEIVED time —
            // stamping `now_ms` here would make every book age zero and
            // blind the freshness gate to feed outages.
            let book_fn = |token: &str| {
                self.books
                    .get(token)
                    .map(|b| crate::exit_policy::BookView::from_book(b, b.received_ms))
            };
            self.positions
                .refresh_marks(&book_fn, now_ms, marks_all, &self.fast_dirty_tokens);
        }
        let (merge_intents, sell_intents): (Vec<_>, Vec<_>) =
            intents.into_iter().partition(|i| i.reason == "merge");
        for intent in merge_intents {
            // (account, token)-keyed: two scopes can hold the same token and a
            // merge intent must burn the pair the intending scope's account owns.
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(condition_id) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .map(|p| p.condition_id.clone())
            else {
                continue;
            };
            let Some(outcome) = self.positions.merge_condition(&condition_id, now_ms) else {
                tracing::info!(
                    condition = %condition_id,
                    "merge intent: no complete pair to burn (one-sided or cross-account) — legs stay open"
                );
                continue;
            };
            if outcome.pairs_merged > Decimal::ZERO {
                // #461: the burned pairs are a RECEIVABLE, not cash. On-chain
                // the CTF merge mints USDC only when the transaction mines;
                // crediting `balance` at intent time said the wallet held
                // money it did not hold — the audit's venue leg (live) or the
                // local cash identity (dry) goes red, correctly. The claim
                // rides the settlement book's journal, so a restart between
                // the burn and the receipt keeps the $1/pair.
                let neg_risk = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.neg_risk_of(&intent.token_id))
                    .unwrap_or(false);
                let claim_id = self.settlement.note_merge_burned(
                    &outcome.condition_id,
                    outcome.account_id.as_str(),
                    // #647: the venue the burned pairs sit on — the legs' own
                    // execution-venue stamp, so the merge request drains to
                    // the executor that can run this CTF merge (and to no
                    // sibling's bridge).
                    &outcome.venue,
                    neg_risk,
                    outcome.pairs_merged,
                    now_ms,
                );
                tracing::info!(
                    condition = %outcome.condition_id,
                    pairs = %outcome.pairs_merged,
                    claim = %claim_id,
                    "merge burned complete pairs — collateral pending on-chain confirmation"
                );
            }
            self.persist_positions();
            for closed in &outcome.closed {
                self.on_position_closed(closed, now_ms);
            }
            tracing::info!(
                condition = %outcome.condition_id,
                pairs = %outcome.pairs_merged,
                cash_usd = %outcome.cash_credited_usd,
                unpaired = %outcome.unpaired_shares,
                "merged complete pairs into collateral"
            );
        }

        // Always re-value open positions from the latest books so the dashboard's
        // unrealized PnL / HWM move even when automated exits are disabled.
        // Replay fast path (--backtest-fast): when EVERY registered strategy
        // holds to settlement and the kernel settles locally, every ladder
        // verdict against an open position is discarded by the settlement-
        // holders filter below — the ladder is pure per-tick waste. The fast
        // path re-values only dirty tokens and skips the ladder + the E31-b
        // scan entirely (a holder never places a working sell, so the
        // residual backstop has nothing to answer between events). A mixed
        // book (any non-holder registered) keeps the full historical pass.
        let all_hold = self.config.backtest_fast
            && self.config.mode.settles_locally()
            && self
                .engine
                .as_ref()
                .is_some_and(|e| e.all_strategies_hold_to_settlement());
        // Issue #390: the scalar projection (BookView) instead of the full
        // snapshot — the ladder reads best_bid/best_ask/mid_price/depths/
        // timestamp and never the level vectors, so the two owned clones and
        // the token String per position per tick disappear; the reductions
        // run once per mirrored book and the Copy result is passed by value.
        // #462: the timestamp is the book's own RECEIVED time (see the marks
        // closure above) — the ladder must not re-arm a stale quote.
        let book_fn = |token: &str| {
            self.books
                .get(token)
                .map(|b| crate::exit_policy::BookView::from_book(b, b.received_ms))
        };
        if all_hold {
            self.positions
                .valuate_dirty_tokens(&book_fn, now_ms, &self.fast_dirty_tokens);
        } else if !self.config.replay_scheduling || self.config.backtest_fast {
            // Legacy spelling (live kernels and plain --backtest-fast without
            // all-hold): the base's every-tick valuate, unchanged.
            //
            // Issue #390 (replay scheduling): this pass is MERGED into the
            // gated ladder below — its state update there runs at the 2×
            // backfill rate (the base advances the counter ratchet twice per
            // tick: once here, once in the ladder), and the mark-only refresh
            // above carries `current_price`/`prev_price`/`last_book_ts`.
            // Running the full valuate too would double the ratchet again on
            // run ticks and is pure idempotent re-observation on skip ticks.
            self.positions.valuate(&book_fn, now_ms);
        }
        let intents = sell_intents;

        // A unit of closing work resolved against a concrete open position.
        #[derive(Clone)]
        struct ExitJob {
            position_id: String,
            token: String,
            condition_id: String,
            price: Decimal,
            reason: ExitReason,
            use_maker: bool,
            internal_key: String,
            strategy: String,
            asset: String,
            direction: String,
            /// E28 (§9.2): the account the closing SELL spends from — the
            /// position's own book, so an exit can never debit another account.
            account_id: AccountId,
            /// Issue #464: shares to sell (`None` = the whole sellable
            /// position, every path's historical behavior). A ladder step's
            /// tranche rides here from the `ExitRequest`.
            close_shares: Option<Decimal>,
        }

        let mut jobs: Vec<ExitJob> = Vec::new();
        let mut has_job: HashSet<String> = HashSet::new();

        // Hold-to-settlement strategies keep their positions to expiry: selling
        // a complete-set leg before redemption destroys the pair's riskless
        // payoff ($1 per share-pair regardless of which side wins). The policy
        // ladder therefore skips them — but only where the kernel settles
        // locally (dry/read-only), where `drive_settlement` does the redemption.
        // On a LIVE kernel the ladder keeps running until venue resolutions are
        // a proven path: a position sitting unmanaged past expiry would be
        // strictly worse than one sold early. Strategy-signal intents still
        // close such positions everywhere — an explicit "get out" is honoured.
        // Fast path: with every strategy a holder, the set is every strategy —
        // and its only consumers (the ladder loop, skipped above) are gated
        // off, so the per-tick rebuild is skipped too.
        let settlement_holders: HashSet<String> = if !all_hold && self.config.mode.settles_locally()
        {
            self.positions
                .open_positions()
                .iter()
                .map(|p| p.strategy.clone())
                .filter(|s| {
                    self.engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(s))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let holds_settlement =
            |pos: &crate::position::OpenPosition| settlement_holders.contains(&pos.strategy);

        if self.config.auto_exits_enabled && !all_hold {
            // Issue #390 (replay nofast): the ladder runs on its deadline
            // schedule instead of every tick — dirty tokens (a book arrived
            // for a held position), the exit-deadline clock (expiry/
            // force-exit/throttle beats), or the 1 s report beat that keeps
            // every time-quantised alert stamp the every-tick pass produced.
            // The nofast replay observes every verdict, so the schedule below
            // is output-preserving by construction: a skipped tick's only
            // durable effect on the ladder is the counter ratchet, which the
            // interval-compensated state update backfills. Every timestamp
            // still stamps the tick that actually fires.
            // `interval_mode` is the #390 merged-maintenance spelling —
            // nofast replay ONLY. The fast path stays on the legacy
            // per-tick `valuate_one`: its all-holder filter discards every
            // ladder verdict anyway, so the merged pass buys it nothing,
            // while the per-position run ledger the merged pass maintains
            // showed up as a measurable regression (68s → 90s full corpus).
            let (ladder_run, interval_mode) = if self.config.backtest_fast {
                (true, false)
            } else if self.config.replay_scheduling {
                (
                    !self.fast_dirty_tokens.is_empty() || now_ms >= self.fast_next_exit_deadline_ms,
                    true,
                )
            } else {
                (true, false)
            };
            for req in self.positions.check_exits_interval(
                &book_fn,
                now_ms,
                ladder_run,
                interval_mode,
                self.config.tick_schedule_ms,
                &self.fast_dirty_tokens,
            ) {
                let Some(pos) = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| p.id == req.position_id)
                    .cloned()
                else {
                    continue;
                };
                if holds_settlement(&pos) {
                    continue;
                }
                has_job.insert(pos.id.clone());
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price: req.exit_price,
                    reason: req.reason,
                    use_maker: req.use_maker,
                    internal_key: exit_intent_key("exit", &pos, &format!("{:?}", req.reason)),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                    // E28 (§9.2): the automated exit spends from the position's
                    // own account.
                    account_id: pos.account_id.clone(),
                    // #464: a ladder step's tranche (None = the whole
                    // position, every no-ladder path unchanged).
                    close_shares: req.close_shares,
                });
            }
        }

        // Strategy-signal exits: resolve the token to a live position and price
        // off its latest valuation, exactly like a manual flatten. The intent's
        // OWN account is the first key (账户↔venue 解耦: two scopes can hold
        // the same token; the close must land on the intending scope's row),
        // and the job's internal key carries the account segment so two
        // accounts' exit ladders never collide in the place-cooldown/dedup
        // map. A legacy intent (no account — the pre-fanout shape) resolves
        // to `default`, the same row it always hit.
        for intent in intents {
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(pos) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .cloned()
            else {
                continue;
            };
            if !has_job.insert(pos.id.clone()) {
                continue; // an automated/policy exit already closes it this cycle
            }
            let price = if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                Decimal::new(1, 2)
            };
            let tag: String = intent
                .reason
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(32)
                .collect();
            jobs.push(ExitJob {
                position_id: pos.id.clone(),
                token: pos.token_id.clone(),
                condition_id: pos.condition_id.clone(),
                price,
                reason: ExitReason::StrategySignal,
                use_maker: false,
                internal_key: exit_intent_key("exit-strategy", &pos, &tag),
                strategy: pos.strategy.clone(),
                asset: pos.asset.clone(),
                direction: pos.direction.as_str().to_string(),
                // E28 (§9.2): the strategy-signal exit spends from the
                // position's own account.
                account_id: pos.account_id.clone(),
                close_shares: None,
            });
        }

        // E31-b residual backstop: a working exit can be outgrown by its
        // position (a late entry fill lands while the sell is resting), and
        // once the sell completes the residual has NO working sell and the
        // exit policy may stay silent (e.g. the profit target already passed).
        // Any position with a sell that filled after the position opened and
        // no live sell now gets an explicit re-close of the remainder.
        // Fast path: a holder never places a working sell on these tokens,
        // so between dirty cycles there is no residual to re-close — the
        // scan is skipped with the ladder (still runs on the non-fast path
        // and whenever any non-holder strategy is registered).
        // Issue #390 (replay scheduling): the scan's inputs move only when an
        // ORDER event lands (a sell fills, a late entry fills) or when the
        // ladder actually ran (its verdicts can place/complete sells) —
        // gating it to those ticks reproduces the every-tick scan: a quiet
        // tick re-reads the same (live sells, filled-since, open positions)
        // and would emit nothing. `fast_orders_dirty` is the #388 order-set
        // wake, maintained on both replay paths.
        let residual_scan = if self.config.backtest_fast {
            true
        } else if self.config.replay_scheduling {
            self.fast_orders_dirty || now_ms >= self.fast_next_exit_deadline_ms
        } else {
            true
        };
        if self.config.auto_exits_enabled && !all_hold && residual_scan {
            // Issue #464: a ladder tranche's SELL is a PARTIAL exit — while it
            // works, the position has shares left on purpose. The residual
            // backstop below re-closes exactly such remainders, so a ladder
            // book would be re-flattened wholesale one tick after every
            // step. A position whose token has a working or recently filled
            // ladder SELL stays out of this scan until the ladder's own next
            // step (or stop) takes it the rest of the way. No-ladder runs
            // never carry tranches, so their scan is untouched.
            let ladder_tranche_tokens: HashSet<String> = jobs
                .iter()
                .filter(|j| j.close_shares.is_some())
                .map(|j| j.token.clone())
                .collect();
            for pos in self.positions.open_positions().to_vec() {
                if !has_job.insert(pos.id.clone()) {
                    continue;
                }
                if ladder_tranche_tokens.contains(&pos.token_id) {
                    continue;
                }
                let has_live_sell = self
                    .ome
                    .live_for(&pos.token_id, Side::Sell)
                    .into_iter()
                    .any(|o| o.status.is_live());
                if has_live_sell {
                    continue;
                }
                let closed_here = self
                    .ome
                    .has_filled_sell_since(&pos.token_id, pos.entered_at_ms);
                if !closed_here {
                    continue;
                }
                let price = if pos.current_price > Decimal::ZERO {
                    pos.current_price
                } else {
                    Decimal::new(1, 2)
                };
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price,
                    reason: ExitReason::ForceExit,
                    use_maker: false,
                    internal_key: exit_intent_key("exit-residual", &pos, ""),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                    // E28 (§9.2): the residual re-close spends from the
                    // position's own account.
                    account_id: pos.account_id.clone(),
                    close_shares: None,
                });
            }
        }

        for job in jobs {
            let already_live = self
                .ome
                .live_for(&job.token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live());
            if already_live {
                continue;
            }
            // The venue already refused this exit a moment ago: let the
            // backoff run instead of re-POSTing every tick (the 380+-rejection
            // storm this guards against). Silent skip — the rejection was
            // already accounted for when it happened.
            if self.placement_blocked(&job.internal_key, now_ms) {
                continue;
            }
            // #464: a ladder tranche sells exactly its floor-to-grid size
            // (never more than the position still holds); `None` sells the
            // whole sellable position — every no-ladder path, unchanged.
            let size = if let Some(tranche) = job.close_shares {
                let Some(whole) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                tranche.min(whole)
            } else {
                let Some(size) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                size
            };
            let mode = if job.use_maker {
                FillPolicy::Maker
            } else {
                FillPolicy::Taker
            };
            // A FOK sell must fill in FULL at or above its limit; priced at
            // the touch it dies on the first thin tick (thin books race the
            // quote). Selling LOWER only widens the marketable range — the
            // venue still fills at the bid or better, so the slip is a floor
            // for where we agree to sell, not a price we pay.
            //
            // #496: a resting (maker) sell priced AT the bid is a crossing
            // post-only order — the venue refuses it every time, so a live TP
            // exit could never rest (the rejection storm #163 only backs off;
            // dry's close-intent exemption masked it). The resting sell goes
            // one tick ABOVE the bid, clamped to stay strictly inside the
            // ask (a price at/above the ask would cross the other side and
            // be refused too). No bid or no book: there is nothing to rest
            // against, so the job's judged price stands and the venue's own
            // refusal (or the dry book aging) reports it.
            let price = if mode == FillPolicy::Taker {
                marketable_exit_price(job.price, self.config.exit_taker_slip_ticks)
            } else {
                self.maker_exit_price(&job.token, job.price)
            };
            let order = OrderRequest {
                token_id: job.token.clone(),
                condition_id: job.condition_id,
                side: Side::Sell,
                mode,
                price,
                size,
                internal_key: job.internal_key,
                strategy: job.strategy,
                asset: job.asset,
                direction: job.direction,
                round_slot: 0,
                // E28 (§9.2): the closing SELL spends from the position's own
                // account (carried on the job from the position).
                account_id: job.account_id,
            };
            // Record the intended exit reason so a full sell fill closes with it
            // (bounded and cleaned up by the close: issue #190).
            self.note_exit_reason(&job.token, job.reason);
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            }
        }
        Ok(())
    }
}
