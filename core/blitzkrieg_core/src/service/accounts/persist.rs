//! accounts.toml persisting writers, scope rebuild and account lifecycle (#780).

use crate::model::*;
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use std::collections::HashSet;

use crate::service::build_lua_scope;

use super::Core;

impl Core {
    /// The account↔venue binding write (`account.venue.set`): the in-memory
    /// write through [`crate::account::AccountLedgers::set_venue`] and the
    /// durable write into the account book's own file are ONE transaction —
    /// the persist happens FIRST (plan-and-persist before apply: a persist
    /// failure leaves both the file and the memory exactly as they were, so
    /// the two stores can never disagree), the in-memory record moves only
    /// after the file accepted. A core without an
    /// [`CoreConfig::accounts_config_path`] (backtester, most in-process
    /// tests) refuses instead of writing into whatever checkout it happens
    /// to run from — the `runtime_config_path` posture, kept.
    ///
    /// Validation of the venue NAME (a registered plugin, or empty = unbind)
    /// is the caller's job — it needs the compiled-in registry, which the
    /// market-agnostic core deliberately cannot see.
    pub fn set_account_venue_persisting(
        &mut self,
        id: &crate::account::AccountId,
        venue: &str,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::schema::ACCOUNT_VENUE_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.venue.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_venue(&legacy, &env_path, id.as_str(), venue)
            .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.venue.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_venue(id, venue, now_ms)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            venue = %venue,
            reason = %reason,
            "account venue binding written — orders.place is gated by it \
             immediately{}; the binding re-reads at next boot \
             ({ACCOUNT_VENUE_SET_NOTE})",
            if venue.is_empty() { " (empty venue = unbound)" } else { "" },
        );
        Ok(())
    }

    /// The account round-FAMILY write (`account.rounds.set`, 轮次多族 B2):
    /// validate → persist FIRST (a failure leaves both stores untouched) →
    /// the in-memory book. The venue.set transaction posture, kept: the
    /// persist happens FIRST (plan-and-persist before apply — the two stores
    /// can never disagree), and a core without an
    /// [`CoreConfig::accounts_config_path`] refuses instead of writing into
    /// whatever checkout it happens to run from.
    ///
    /// Validation reuses [`crate::account::config::validate_rounds`] (the
    /// SAME whitelist + dedup the parser runs — one spelling of the rule,
    /// never two) over `Some(rounds)`; an empty list passes through as the
    /// legal unrestricted set and REMOVES the file key (missing == `[]` at
    /// the next boot; the deleted key is the cleaner spelling). The
    /// open-exposure refusal for a shrinking set is the IPC arm's job — it
    /// needs the position book AND the removed-family arithmetic, and its
    /// refusal must move NOTHING (memory and disk exactly as they were).
    pub fn set_account_rounds_persisting(
        &mut self,
        id: &crate::account::AccountId,
        rounds: Vec<i64>,
        actor: &str,
        reason: &str,
        _now_ms: i64,
    ) -> CoreResult<()> {
        use crate::schema::ACCOUNT_ROUNDS_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        // One validation, the parser's own: the plugins' declared union +
        // dedup, first-seen order kept. `Some(..)` (not `None`) — the wire
        // asked for a set.
        let rounds =
            crate::account::config::validate_rounds(Some(rounds), &self.config.allowed_round_secs)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.rounds.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_rounds(
            &legacy,
            &env_path,
            id.as_str(),
            &rounds,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.rounds.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        // The write-side re-check inside `set_rounds` runs again (same
        // fail-closed posture at every door) — it cannot refuse what
        // `validate_rounds` passed, but the door stays.
        self.accounts
            .set_rounds(id, rounds.clone(), &self.config.allowed_round_secs)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            rounds = ?rounds,
            reason = %reason,
            "account round families written — the (venue, duration) admission \
             gate reads the new set immediately{}; the declaration re-reads at \
             next boot ({ACCOUNT_ROUNDS_SET_NOTE})",
            if rounds.is_empty() {
                " (empty set = unrestricted, key removed from accounts.toml)"
            } else {
                ""
            },
        );
        Ok(())
    }

    /// The arb leg configuration in force (`arb.config.get`'s data source).
    /// `None` = unconfigured.
    pub fn arb_leg_config(&self) -> Option<&crate::account::CrossVenueArbConfig> {
        self.arb_leg_config.as_ref()
    }

    /// The arb leg configuration write (`arb.config.set`): validate → persist
    /// FIRST (a failure leaves both stores untouched) → memory → hot-push the
    /// engine. The hot-push is the whole runtime effect: the adapter stamps
    /// entry/exit accounts per token venue, and the strategy reads its
    /// legs/pairs from the `bk.params()` bag merged on the next evaluate.
    ///
    /// Validations (fail-closed, every refusal names its field):
    ///   * both venues non-empty AND distinct;
    ///   * both accounts exist in the book (accounts.toml's `[[account]]`s);
    ///   * when an account carries a venue binding, that binding must admit
    ///     its own leg's venue (an account pinned to venue V cannot trade
    ///     leg venue W — the scope gate would refuse every leg);
    ///   * pairs non-empty.
    pub fn arb_config_set_persisting(
        &mut self,
        cfg: &crate::account::CrossVenueArbConfig,
        actor: &str,
        reason: &str,
        _now_ms: i64,
    ) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        self.arb_config_validate(cfg)?;
        // Persist first: the file's parse layer re-validates coherence (the
        // same rules the boot runs), so a write that would produce a
        // next-boot-refusing file is refused HERE, before memory moved.
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(err(
                "arb.config.set: no accounts config path configured — persistence is \
                 disabled for this core (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_cross_venue_arb(&legacy, &env_path, cfg).map_err(
            |e| {
                err(format!(
                    "arb.config.set: persist failed, nothing changed: {}",
                    e.message
                ))
            },
        )?;
        // Memory half, then the hot-push: the engine's adapters read the
        // stamp source, and the strategy reads the params bag — both on the
        // next evaluate, no restart.
        self.arb_config_apply(cfg);
        let (leg_a_venue, leg_a_account, leg_b_venue, leg_b_account) = (
            cfg.leg_a_venue.trim(),
            cfg.leg_a_account.trim(),
            cfg.leg_b_venue.trim(),
            cfg.leg_b_account.trim(),
        );
        tracing::info!(
            target: "risk",
            actor = %actor,
            leg_a = %format!("{leg_a_venue}/{leg_a_account}"),
            leg_b = %format!("{leg_b_venue}/{leg_b_account}"),
            pairs = ?self.arb_leg_config.as_ref().expect("just set").pairs,
            reason = %reason,
            "cross-venue arb legs written — the account stamp and the pairing params are \
             in force on the next evaluation; the section re-reads at next boot"
        );
        Ok(())
    }

    /// The fail-closed validations `arb.config.set` runs BEFORE anything
    /// moves (shared by the persisting write and the memory-only write —
    /// every refusal names its field).
    pub(super) fn arb_config_validate(
        &self,
        cfg: &crate::account::CrossVenueArbConfig,
    ) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        let leg_a_venue = cfg.leg_a_venue.trim();
        let leg_b_venue = cfg.leg_b_venue.trim();
        if leg_a_venue.is_empty() {
            return Err(err(
                "arb.config.set: legAVenue must be a non-empty venue name".into(),
            ));
        }
        if leg_b_venue.is_empty() {
            return Err(err(
                "arb.config.set: legBVenue must be a non-empty venue name".into(),
            ));
        }
        if leg_a_venue == leg_b_venue {
            return Err(err(format!(
                "arb.config.set: legBVenue `{leg_b_venue}` is the same venue as legAVenue — \
                 the two legs must sit on DIFFERENT venues"
            )));
        }
        let leg_a_account = cfg.leg_a_account.trim();
        let leg_b_account = cfg.leg_b_account.trim();
        for (field, id) in [
            ("legAAccount", leg_a_account),
            ("legBAccount", leg_b_account),
        ] {
            let account = self
                .accounts
                .account(&crate::account::AccountId::from(id))
                .ok_or_else(|| {
                    err(format!(
                        "arb.config.set: {field} `{id}` names no configured account — \
                         configured accounts are: {}",
                        self.accounts
                            .account_list()
                            .iter()
                            .map(|a| a.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            let bound = account.venue.trim();
            let leg_venue = if field == "legAAccount" {
                leg_a_venue
            } else {
                leg_b_venue
            };
            if !bound.is_empty() && bound != leg_venue {
                return Err(err(format!(
                    "arb.config.set: {field} `{id}` is bound to venue `{bound}` — it cannot \
                     trade leg venue `{leg_venue}` (unbind it in accounts.toml or pick another \
                     account)"
                )));
            }
        }
        if cfg.pairs.is_empty() {
            return Err(err(
                "arb.config.set: pairs must name at least one asset — an empty list pairs \
                 nothing"
                    .into(),
            ));
        }
        // The pair list is a SET: a duplicate entry would double-push the
        // same asset's pairing (and a next boot would refuse the same file —
        // keep the set semantics identical to `validate_cross_venue_arb_section`).
        let mut seen_pairs = std::collections::HashSet::new();
        for p in &cfg.pairs {
            let up = p.trim().to_uppercase();
            if up.is_empty() {
                return Err(err(
                    "arb.config.set: pairs must not contain a blank asset name".into(),
                ));
            }
            if !seen_pairs.insert(up) {
                return Err(err(format!(
                    "arb.config.set: pairs `{}` appears twice — the list is a set",
                    p.trim()
                )));
            }
        }
        Ok(())
    }

    /// The memory half of a validated set: the normalized config into the
    /// Core cell, hot-pushed to the engine's adapters (the stamp source and
    /// the `bk.params()` entries both read it on the next evaluate).
    pub(super) fn arb_config_apply(&mut self, cfg: &crate::account::CrossVenueArbConfig) {
        self.arb_leg_config = Some(crate::account::CrossVenueArbConfig {
            leg_a_venue: cfg.leg_a_venue.trim().to_string(),
            leg_a_account: cfg.leg_a_account.trim().to_string(),
            leg_b_venue: cfg.leg_b_venue.trim().to_string(),
            leg_b_account: cfg.leg_b_account.trim().to_string(),
            pairs: cfg
                .pairs
                .iter()
                .map(|p| p.trim().to_uppercase())
                .collect::<Vec<_>>(),
        });
        if let Some(e) = self.engine.as_mut() {
            e.set_arb_leg_config(self.arb_leg_config.clone());
        }
    }

    /// The MEMORY-ONLY leg-configuration write: the same validations and the
    /// same hot-push as [`Self::arb_config_set_persisting`], without the file.
    /// The shape a core without an `accounts.toml` path takes (backtests and
    /// in-process tests) — a restart forgets it, exactly like every other
    /// in-memory knob those cores run.
    pub fn arb_config_set(&mut self, cfg: &crate::account::CrossVenueArbConfig) -> CoreResult<()> {
        self.arb_config_validate(cfg)?;
        self.arb_config_apply(cfg);
        Ok(())
    }

    /// 配对声明受理（engine 提交管线在 arb_precheck 过闸后逐腿调用）。
    /// 经济身份 (asset, slot) 从腿 token 经 engine scanner 的 token→market
    /// 表派生（不是策略自报的）；token 不在本轮任何市场 ⇒ slot 派生不出
    /// ⇒ 拒收（fail-closed）。owner_account 是实例主账户（实例隔离维度），
    /// 策略自报 key 只作可读标签。声明载荷整体从 engine 的 [`crate::engine::PairDeclaration`]
    /// 传入（owner/腿账户/token/shares 一个结构 —— 受理面不再逐参重抄）。
    pub(crate) fn pair_declare_leg(&mut self, d: &crate::engine::PairDeclaration, now_ms: i64) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        // token→market 表：slot/asset 的派生权威（engine scanner 本轮市场）。
        let (asset, slot) = match engine.round_slot_of(&d.token_id) {
            Some(slot) if slot > 0 => (engine.asset_of(&d.token_id).unwrap_or_default(), slot),
            _ => return, // 不在本轮任何市场：无轮次身份，不配进簿
        };
        let facts = crate::pair::TokenMarketFacts { asset, slot };
        let decl = crate::pair::PairLegDecl {
            token_id: d.token_id.clone(),
            shares: d.shares.unwrap_or(Decimal::ZERO),
            leg: d.decl.leg.clone(),
            // 盖章结果（v2 可 A≠B）——PairBook 的腿账户与投影都以它为准。
            leg_account: d.leg_account.clone(),
        };
        let _ = self.pair_book.declare(
            &d.owner_strategy,
            &d.owner_account,
            &d.decl.key,
            &decl,
            &facts,
            now_ms,
        );
    }

    /// place 成功后的 internal_key 回填（OME 事实事件经它寻址回声明腿）。
    pub(crate) fn pair_bind_key(
        &mut self,
        strategy: &str,
        owner_account: &str,
        token_id: &str,
        internal_key: &str,
    ) {
        self.pair_book
            .bind_by_leg(strategy, owner_account, token_id, internal_key);
    }

    /// OME 事实事件喂入（成交/死亡）——幂等，未命中声明腿的事件是空操作。
    pub(crate) fn pair_on_order_event(
        &mut self,
        internal_key: &str,
        filled_delta: Decimal,
        dead: bool,
    ) {
        self.pair_book
            .on_order_event(internal_key, filled_delta, dead);
    }

    /// 轮次推进清退：换轮臂（engine_on_data 的 RoundMarkets 非 top-up）调用。
    pub(crate) fn pair_retire_before(&mut self, before: i64) {
        self.pair_book.retire_before(before);
    }

    /// `bk.arb.pairs()` 投影的数据源：owner 过滤的配对视图（跨策略/跨账户
    /// 不可见）。`accounts` 是该实例可见的账户白名单（实例主账户 + arb 腿
    /// 账户，与 holdings 视图的拓宽同形）。
    pub(crate) fn pair_views_for(
        &self,
        strategy: &str,
        accounts: &HashSet<String>,
    ) -> Vec<crate::pair::PairView> {
        self.pair_book.views_for(strategy, accounts)
    }

    /// 配对簿的测试/审计只读快照（无过滤）——`pair_book_check.rs` 经它断言
    /// 受理链的落账结果；生产路径一律走 `pair_views_for`（owner 过滤投影）。
    pub fn pair_book_all(&self) -> Vec<crate::pair::PairState> {
        self.pair_book.all().into_iter().cloned().collect()
    }

    /// Unset the arb leg configuration (the panel's 「解除配置」): remove the
    /// file section and the memory mirror, engine hot-pushed. `arb.config.set`
    /// is the only writer in the v2 flow; this is the delete arm the wire
    /// spells with `configured: false` after a successful call.
    pub fn arb_config_clear_persisting(&mut self, actor: &str) -> CoreResult<()> {
        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(err(
                "arb.config.clear: no accounts config path configured — persistence is \
                 disabled for this core (backtests and in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_clear_cross_venue_arb(&legacy, &env_path).map_err(|e| {
            err(format!(
                "arb.config.clear: persist failed, nothing changed: {}",
                e.message
            ))
        })?;
        self.arb_leg_config = None;
        if let Some(e) = self.engine.as_mut() {
            e.set_arb_leg_config(None);
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            "cross-venue arb legs cleared — pairing falls back to the unified-confirm chain"
        );
        Ok(())
    }

    /// The trading-switch write (`account.trading.set`): memory AND
    /// accounts.toml in one transaction — persist FIRST (a failure leaves
    /// both stores untouched), memory after. The engine mirror needs no
    /// separate push here: the next evaluation cycle re-pushes every
    /// account's trading fact, so the gate is in force within one cycle.
    /// `default` is settable like any account (a single-book deployment may
    /// want its one account's entries off too).
    pub fn set_account_trading_persisting(
        &mut self,
        id: &crate::account::AccountId,
        enabled: bool,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::schema::ACCOUNT_TRADING_SET_NOTE;
        self.accounts.require(id)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.trading.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_trading(
            &legacy,
            &env_path,
            id.as_str(),
            enabled,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.trading.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_trading(id, enabled, now_ms)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            enabled = %enabled,
            reason = %reason,
            "account trading switch written — {} re-reads at next boot \
             ({ACCOUNT_TRADING_SET_NOTE})",
            if enabled { "entries re-open; " } else { "new entries refused (exits flow); " },
        );
        Ok(())
    }

    /// The display-pointer write (显示账户配置化 / `display.account.set`):
    /// memory AND accounts.toml in one transaction — persist FIRST (a failure
    /// leaves both stores untouched), memory after. The refusals are ALL
    /// plan-first: the id must name a CONFIGURED account.
    ///
    /// 2026-10-20 操作者裁决（issue 561）：display 是面板的唯一可信账户
    /// 事实源，切户动作 = 写 display。校验据此放宽为「账户存在即可」，
    /// 不再要求 `status = Active`。display 是显示焦点而非交易授权面：
    /// 把指针指向 frozen / read_only 账户去看它的账本与流水是合法操作
    /// （与 `account.switch` 的 existence-only 姿态同一逻辑 —— posture
    /// 管下单，不管看）。消费侧（行情过滤、策略归属、风控、下单）各自
    /// 按账户自身 posture 做门禁，display 指针不放行任何交易语义。
    ///
    /// 语义边界：this pointer never touches the process-level active
    /// account, no ledger, and no strategy enablement — the snapshot's
    /// `displayAccount`/`displayVenue` (and the issue-561 default chain)
    /// are the only readers.
    pub fn set_display_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        actor: &str,
        reason: &str,
    ) -> CoreResult<()> {
        use crate::schema::DISPLAY_ACCOUNT_SET_NOTE;
        self.accounts.require(id)?;
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "display.account.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_display_account(&legacy, &env_path, id.as_str())
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "display.account.set: persist failed, nothing changed: {}",
                        e.message
                    ),
                )
            })?;
        self.accounts.set_display(id)?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            reason = %reason,
            "display pointer written — the snapshot's displayAccount/ \
             displayVenue follow this account; re-reads at next boot \
             ({DISPLAY_ACCOUNT_SET_NOTE})",
        );
        Ok(())
    }

    /// The strategy-assignment write (`account.strategies.set`): memory AND
    /// accounts.toml in one transaction, plus the engine scope rebuild.
    ///
    /// Names are validated against the engine's REGISTERED set (plan-first —
    /// a typo fails here, never as a silently inert assignment). The rebuild:
    /// every (account, strategy) scope NOT in the new assignment and holding
    /// NO open position for this account is torn down (Drop); one with open
    /// positions is kept as an EXITS-ONLY instance (enabled stays whatever it
    /// was, but the engine drops its entry candidates for this account from
    /// the next cycle on — see `Engine::scope_rebuild`) until the account is
    /// flat in it, then a later call (or boot) drops it. New names in the
    /// assignment are instantiated from the SAME lua package the default
    /// scope loaded (own Lua state, own book).
    ///
    /// 账户↔venue 解耦语义: `requested` is the account's strategy
    /// ENABLEMENT set (the 启用按钮's persistence), not an assignment or a
    /// visibility filter — strategies are global. The write is a whole-set
    /// decision: memory AND accounts.toml in one transaction (persist
    /// first), then the engine scopes rebuilt from it (removed-and-flat
    /// drop; removed-and-held demote exits-only; added instantiate enabled).
    ///
    /// Returns the exits-only keep-list for the reply.
    pub fn set_account_strategies_persisting(
        &mut self,
        id: &crate::account::AccountId,
        strategies: Vec<String>,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<Vec<String>> {
        use crate::schema::ACCOUNT_STRATEGIES_SET_NOTE;
        self.accounts.require(id)?;
        // Dedupe + order-stable: an assignment that names a strategy twice
        // is a caller bug, but the honest handling is collapse, not refusal.
        let mut requested: Vec<String> = Vec::new();
        for s in &strategies {
            let name = s.trim();
            if name.is_empty() {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    "account.strategies.set: an empty strategy name in the \
                     list cannot be addressed"
                        .to_string(),
                ));
            }
            if !requested.iter().any(|r| r == name) {
                requested.push(name.to_string());
            }
        }
        // Plan-first validation: every requested name must be REGISTERED in
        // the engine (any scope). The registered set is the loadable set —
        // the default scope hosts every loaded package.
        if self.engine.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.strategies.set: no strategy engine attached".to_string(),
            ));
        }
        for name in &requested {
            if !self
                .engine
                .as_ref()
                .expect("engine presence checked above")
                .strategy_registered(name)
            {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account.strategies.set: strategy `{name}` is not \
                         registered — the assignable set is what \
                         `strategy.list` reports"
                    ),
                ));
            }
        }
        // Positions THIS account holds, by strategy — the exits-only safety
        // net's input.
        let held: Vec<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *id)
            .map(|p| p.strategy.clone())
            .collect();
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.strategies.set: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_strategies(
            &legacy,
            &env_path,
            id.as_str(),
            &requested,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.strategies.set: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts
            .set_strategies(id, requested.clone(), now_ms)?;
        // #537: the default account's writes keep the persisted enabled-set
        // in lockstep. The boot migrates the state file's set into the file
        // ONLY while the default account has no `strategies` key; without
        // this mirror, a set the operator emptied (key removed from
        // accounts.toml) would be re-imported from a stale state file at the
        // next boot — the exact resurrection the default-toggle mirror above
        // closes. Non-default accounts never migrate, so their state file is
        // untouched (the global toggle's file stays theirs alone).
        if *id == crate::account::default_account_id()
            && let Some(state_path) = self.config.strategy_state_path.as_deref()
        {
            crate::strategy_state::save(std::path::Path::new(state_path), &requested);
        }
        // Engine rebuild. Removed-and-flat scopes drop; removed-and-held
        // scopes are marked exits-only; added scopes instantiate. An EMPTY
        // assignment is explicit "this account runs no strategies" — the
        // rebuild then drops every flat scope of the account (the INHERIT
        // reading this used to carry was retired: per-account assignments
        // are the only truth, the default account included).
        let kept_exits_only = self.rebuild_account_scopes(id, &requested, &held);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            strategies = ?requested,
            exits_only = ?kept_exits_only,
            reason = %reason,
            "account strategy assignment written — engine scopes rebuilt, \
             re-read at next boot ({ACCOUNT_STRATEGIES_SET_NOTE})",
        );
        Ok(kept_exits_only)
    }

    /// The engine half of an enablement-set write: delegate to the engine's
    /// own scope rebuild, handing it a closure that re-instantiates Lua
    /// packages (the only rebuildable source) from their dir. Returns the
    /// names kept EXITS-ONLY for this account.
    pub(crate) fn rebuild_account_scopes(
        &mut self,
        id: &crate::account::AccountId,
        requested: &[String],
        held: &[String],
    ) -> Vec<String> {
        let account = id.as_str().to_string();
        let Some(engine) = self.engine.as_mut() else {
            return Vec::new();
        };
        // NOTE: `build` borrows `self` mutably while `engine` is borrowed —
        // impossible directly, so the lua re-loader is a FREE function over
        // captured state (name + source dir come from the DEFAULT scope's
        // registration, read before the engine borrow opens).
        let sources: std::collections::HashMap<String, String> =
            engine.scope_sources().into_iter().collect();
        let mut build = |name: &str, account: &str| build_lua_scope(&sources, name, account);
        engine.rebuild_account_scopes(&account, requested, held, &mut build)
    }

    /// Boot application of the accounts file's strategy ENABLEMENT sets
    /// (账户↔venue 解耦语义) — the production `run()` calls this ONCE after
    /// `install_engine`. For every account with a NON-EMPTY `strategies` key
    /// the named scopes are instantiated and ENABLED; the default account
    /// with NO key and a non-empty persisted enabled-set (strategy-state
    /// .json — the old global toggle's durable form) is MIGRATED once: that
    /// set is materialized as the default account's enablement set in
    /// accounts.toml, so the upgrade keeps the production behaviour exactly
    /// (default 账户 strategies=["pair_discount_arb","spread_arb"] 升级后
    /// 行为不变) and the file is the only truth from that boot on.
    ///
    /// NOT part of `install_engine`: the replay/backtest boot goes through
    /// that path too and its single-default book must keep byte-identical
    /// outcomes (the exit-economics baseline). Production assembly is the
    /// only caller.
    ///
    /// Returns the migrated default enablement set (empty when nothing
    /// migrated) so the caller can report it on stderr.
    pub fn apply_account_strategy_assignments_at_boot(&mut self) -> Vec<String> {
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Vec::new();
        };
        // The persisted enabled-set (strategy-state.json) drives the one-time
        // migration of the default account. Read BEFORE any write.
        let persisted_enabled: Vec<String> = match self.config.strategy_state_path.as_deref() {
            Some(p) => crate::strategy_state::load(std::path::Path::new(p)),
            None => Vec::new(),
        };
        let default_id = crate::account::default_account_id();
        // The default account's assignment key decides whether the boot runs
        // on the file's spelling (migration already happened) or migrates.
        let default_has_key = self
            .accounts
            .account(&default_id)
            .is_some_and(|a| !a.strategies.is_empty());
        let mut migrated: Vec<String> = Vec::new();
        if !default_has_key && !persisted_enabled.is_empty() {
            // One-time migration: the file gains the explicit default
            // assignment, spelling exactly what the old implicit behaviour
            // (inherit the persisted enabled set) produced. The write is
            // best-effort — a read-only tree keeps the implicit shape and
            // the boot proceeds (the persisted set is re-read next boot).
            //
            // #537: the persisted set is also kept in lockstep from here on —
            // the default toggle now rewrites BOTH stores, and a set the
            // operator emptied (key removed from the file, empty state file)
            // must not be resurrected by a stale state file left over from
            // the pre-toggle era. This one-time gate fires only when the file
            // never spelled an assignment; afterwards the accounts file is
            // the sole truth, exactly as the contract above states.
            if crate::account::config::env_persist_account_strategies(
                &legacy,
                &env_path,
                default_id.as_str(),
                &persisted_enabled,
            )
            .is_ok()
            {
                let _ =
                    self.accounts
                        .set_strategies(&default_id, persisted_enabled.clone(), now_ms());
                migrated = persisted_enabled;
                tracing::info!(
                    strategies = ?migrated,
                    "strategy enablement migration: the default account's persisted \
                     enabled set is materialized as its accounts.toml enablement set — \
                     the file is the only truth from here on"
                );
            }
        }
        // Apply every account's enablement set (the default's included —
        // after migration it spells the old behaviour). An empty key is the
        // EXPLICIT "run nothing on this account" set — it never follows the
        // global toggle (INHERIT is retired), so nothing is fanned out here
        // and the account's scopes stay silent until the operator enables
        // strategies for it on the strategies page.
        let assignments: Vec<(String, Vec<String>)> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| !a.strategies.is_empty())
            .map(|a| (a.id.as_str().to_string(), a.strategies.clone()))
            .collect();
        for (account, strategies) in assignments {
            // A set member's scope is built ENABLED — being in the account's
            // enablement set IS the operator's intent to run it there.
            // Unknown names are skipped with a warning by the rebuild (the
            // registered set is what the strategy dir loaded this boot).
            self.set_account_scopes_at_boot(&account, &strategies);
        }
        migrated
    }

    /// The boot-flavoured scope application: instantiate `strategies` for
    /// `account` and enable every one that landed. No exits-only handling —
    /// at boot the position book is restored AFTER this runs, so nothing is
    /// held yet (the restored positions reattach to the rebuilt scopes by
    /// name; a position whose strategy is not in the account's assignment
    /// keeps getting exit-managed because the position book drives exits, not
    /// the scope set).
    pub(super) fn set_account_scopes_at_boot(&mut self, account: &str, strategies: &[String]) {
        // The engine borrow and the Lua re-loader cannot overlap: read the
        // source map first (same shape as `rebuild_account_scopes`).
        let sources: std::collections::HashMap<String, String> = self
            .engine
            .as_ref()
            .map(|e| e.scope_sources().into_iter().collect())
            .unwrap_or_default();
        let mut build = |name: &str, acct: &str| build_lua_scope(&sources, name, acct);
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        // DEMOTE/DROP phases are irrelevant at boot (no held positions yet);
        // the rebuild's ADD phase instantiates + enables.
        engine.rebuild_account_scopes(account, strategies, &[], &mut build);
    }

    /// `account.create`（账户簿运行时增删的建半）：新账户落内存 AND
    /// accounts.toml，一个事务 —— persist 先行（失败则两个存储都不动），文件
    /// 收下后才动内存，与 [`Self::set_account_venue_persisting`] 同一姿态。
    /// 身份合同在这里钉死：id 非空 / trim 后非空 / 不允许保留名 `default` /
    /// 不允许与簿内现有 id 重复；status 恒 `Active`（运行时铸造一个受限账户
    /// 等于变相放宽，收紧是 `account.status` 的事）；market_type 恒
    /// `Prediction`（0.3 唯一有插件的市场类型，与文件缺省一致）。调用方从
    /// 插件声明传入凭据变量名并与身份一起落盘；值仍由
    /// `market.credentials.set` 写入，永不进入账户请求或响应。
    /// 无 [`CoreConfig::accounts_config_path`] 的内核（回测、多数进程内测试）
    /// 拒绝 —— 同 venue.set 的 "persistence is disabled" 文案形状。
    ///
    /// venue 名的注册校验（plan-first）是调用方的职责 —— 编译进来的插件注册
    /// 表在 IPC 层，市场无关的 core 看不见（同 [`Self::set_account_venue_persisting`]）。
    pub fn create_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        name: Option<&str>,
        venue: Option<&str>,
        credential_keys: crate::account::CredentialKeys,
        audit: (&str, &str),
        now_ms: i64,
    ) -> CoreResult<()> {
        use crate::schema::ACCOUNT_CREATE_NOTE;
        let (actor, reason) = audit;
        // Reserved name first — BEFORE the empty-id check, so the refusal
        // names the contract, not the spelling (`"default"` trimmed and
        // whitespace-padded variants land on the same reserved refusal).
        let trimmed = id.as_str().trim();
        if trimmed == crate::account::DEFAULT_ACCOUNT_ID {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: id `default` is reserved — the zero-config \
                 deployment and the active-cell fallback both own it; add your \
                 accounts under their own ids"
                    .to_string(),
            ));
        }
        if trimmed.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: id is empty — an account without an identity \
                 cannot be addressed or audited"
                    .to_string(),
            ));
        }
        let id = crate::account::AccountId::from(trimmed);
        // Existence (duplicate) with an honest refusal, before disk.
        if self.accounts.account(&id).is_some() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("account.create: account `{id}` already exists"),
            ));
        }
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.create: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_create(
            &legacy,
            &env_path,
            id.as_str(),
            name,
            venue,
            &credential_keys,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.create: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.insert(crate::account::Account {
            id: id.clone(),
            name: name.unwrap_or(id.as_str()).to_string(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys,
            venue: venue.filter(|v| !v.is_empty()).unwrap_or("").to_string(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: now_ms,
        })?;
        // A runtime mint on a locally-settling core gets the same seed a boot
        // account would have woken up with. Without this the strategy sizing
        // reads equity 0 and the scope never trades (`policy.skip: equity 0
        // below min_equity_usd`) — and no boot will ever repair it: the boot
        // seed (`seed_all`) is skipped wholesale once ANY account has a
        // journal, which an established deployment always does. LIVE is not
        // seeded: the venue's real cash is the balance, realigned from the
        // venue report. The journal row (kind Seed, ref `account.create`)
        // makes the funding a replayable boot fact for the NEXT boot.
        //
        // The journal only lands if the minted ledger has a journal PATH —
        // boot's `attach_journals` never sees accounts that did not exist at
        // boot, so the path is attached HERE. Without it the funding row stays
        // in the in-memory trail only and a restart wakes the account at zero
        // (生产 probe2 实锤: balance 1000 → restart → 0).
        let ledger = self.accounts.get_mut(&id)?;
        if let Some(dir) = self.config.ledger_journal_dir.as_deref() {
            ledger.set_journal_path(
                std::path::Path::new(dir).join(format!("{}.journal.jsonl", id.as_str())),
            );
        }
        if self.config.mode.settles_locally()
            && self.config.dry_seed_balance > rust_decimal::Decimal::ZERO
        {
            ledger.fund(self.config.dry_seed_balance, "account.create");
        }
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            name = %name.unwrap_or(id.as_str()),
            venue = %venue.unwrap_or(""),
            reason = %reason,
            "account created — ACTIVE prediction account minted and persisted; \
             plugin credential names attached (values ride market.credentials.set) \
             ({ACCOUNT_CREATE_NOTE})",
        );
        Ok(())
    }

    /// `account.remove`（账户簿运行时增删的删半）：账户离开内存 AND
    /// accounts.toml，同一事务姿态。拒绝面：账簿的最后一个账户（空簿会让
    /// 下一次 boot 拒载，删掉它是删库式脚枪）；仍有 open position 的账户
    /// （钱不能变成不可寻址的）；未知 id 标准拒绝。`default` 只是普通账户
    /// —— 所有账户都可删（用户裁决：绑定只在建户时绑定、编辑面已收，
    /// 删户重建是绑定变更的唯一出口；零配置的 default 由下一次 boot 的
    /// 缺文件/缺行路径重新派生）。被删的是进程级 active 账户时自动回切
    /// 剩余账户的第一个（会话级切换器在下次读时照簿解析）。
    pub fn remove_account_persisting(
        &mut self,
        id: &crate::account::AccountId,
        actor: &str,
        reason: &str,
    ) -> CoreResult<()> {
        use crate::schema::ACCOUNT_REMOVE_NOTE;
        // The LAST account stays: an empty book fails the next boot's parse,
        // so retiring it would be a delete-the-deployment footgun. Every id
        // is otherwise fair game — `default` included (the zero-config
        // fallback re-derives on the next boot when the file lacks a
        // `default` row). Existence refuses first, with the standard
        // unknown-account error.
        self.accounts.require(id)?;
        if self.accounts.account_list().len() <= 1 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 是账户簿里最后一个账户——至少要保留一个账户 \
                     （下一次启动会拒绝空账户簿；确要清空部署请直接删除 accounts.toml）"
                ),
            ));
        }
        // Open positions pin the account: retiring it would leave inventory
        // with no book, no ledger and no close path — money must not become
        // unaddressable.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| &p.account_id == id)
            .count();
        if open > 0 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 还有 {open} 笔未平持仓——请先平仓 \
                     （删掉的账户不带走任何库存，钱必须始终有簿可归）"
                ),
            ));
        }
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "account.remove: no accounts config path configured — \
                 persistence is disabled for this core (backtests and \
                 in-process tests); nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_remove(&legacy, &env_path, id.as_str())
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "account.remove：账户簿落盘失败，两个存储都没有改动：{}",
                        e.message
                    ),
                )
            })?;
        // Journal 收尾（现金流水卡与任何账户对不上的根因修复）：把该账户的
        // 磁盘流水文件删掉，内存 trail 同步清空。同名 id 的下一次
        // `account.create` 是新账本，不是老账户续弦 —— 留着旧文件，下一次
        // boot 的 attach_journals 会把上一世的成交流水整表回放进来（生产
        // kalshi 实锤：6 轮建/删叠出 6 笔幽灵 seed@account.create，Σamount
        // 比真实余额多 5000）。删文件失败则整体拒绝 —— 此时 accounts.toml
        // 已收下了删除（persist 先行的代价），但内存未动、旧 journal 还在，
        // 重试一次即可收敛；拒绝的消息如实告知。
        if !self.accounts.get_mut(id)?.retire_journal_file() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "account.remove：`{id}` 的流水文件删除失败 —— accounts.toml \
                     已落盘但内存未动，旧流水文件还在；请重试一次，问题持续 \
                     请检查 data/ledger/ 的写权限"
                ),
            ));
        }
        // issue 723: the bound venues' evidence cursors die with the ledger —
        // a same-name re-create is a NEW journal generation with its own seq
        // space, and the previous generation's row numbers must never judge
        // it (a cursor past every seq the new ledger will ever issue reads
        // booked as 0 forever). Collected BEFORE the book shrinks: the
        // venue→account resolution reads the very binding this remove ends.
        let orphaned_venues: Vec<String> = self
            .venue_fact_cursor
            .keys()
            .filter(|v| self.venue_cash_account(v.as_str()).as_ref() == Some(id))
            .cloned()
            .collect();
        self.accounts.remove(id)?;
        for v in orphaned_venues {
            self.venue_fact_cursor.remove(&v);
        }
        // 冻结簿随账户走：旧实现留着悬空冻结项（同名 id 重建即继承上一世
        // 的 kill switch），自愈时间戳同理一并摘除。
        self.freeze_book.account_freezes.remove(id.as_str());
        self.freeze_book.auto_frozen.remove(id.as_str());
        self.accounts.repair_active_after_removal(id);
        // 显示账户配置化: the removal must not orphan the display pointer —
        // the file was rewritten WITHOUT the row above, and the same render
        // re-derived its top-level `display_account` key when it pointed here
        // (persist_account_remove's own edit); the memory cell re-derives the
        // same way so the snapshot never names a removed account.
        self.accounts.repair_display_after_removal(id);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            reason = %reason,
            "account removed — identity and ledger retired, the active cell \
             re-activates the first surviving account when it named the \
             removed account; the display pointer re-derives the same way \
             when the displayed account left ({ACCOUNT_REMOVE_NOTE})",
        );
        Ok(())
    }
}
