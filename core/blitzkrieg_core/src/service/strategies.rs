//! The strategy/plugin surface, moved verbatim from `service.rs` (pure code
//! motion, no logic edits): strategy loading/unloading and the Lua package
//! scanner, the enable/scope/compat handshake (E27), hot-parameter writes,
//! the shadow-evolution lifecycle, the unified cross-venue mapping ledger's
//! IPC verbs, the extension registry listing and the trade-summary reports.

use crate::model::*;
use crate::ome::Ome;
use crate::position::PositionManager;
use crate::schema::Event;
use crate::shadow_evolution::{EvolutionOutcome, EvolutionStatus, MutableParams, ShadowEvolution};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;

use super::Core;
use super::config::CoreConfig;
use super::helpers::ARB_STRATEGY;
use super::{confirm_error_text, listing_json};

impl Core {
    pub fn ome(&self) -> &Ome {
        &self.ome
    }

    pub fn positions(&self) -> &PositionManager {
        &self.positions
    }

    /// Mutable access to the position manager — TEST SEAM ONLY (test cfg or
    /// feature `test-support`; #780/#825 boundary hygiene, the same gate
    /// [`Self::ledger_mut`] carries). The #364 IPC tests seed closed rows to
    /// replay. Not for moving money: every cash path routes through the
    /// ledgers, not this book — and no production caller may reach the book
    /// bluntly.
    #[cfg(any(test, feature = "test-support"))]
    pub fn positions_mut(&mut self) -> &mut PositionManager {
        &mut self.positions
    }

    /// Mutable handle on the account book — TEST SEAM ONLY (test cfg or
    /// feature `test-support`; #780/#825 boundary hygiene, the same gate
    /// [`Self::ledger_mut`] carries). The #364 IPC tests prime a ledger
    /// before replaying history. The LIVE money paths never come through
    /// here: they route by the order's own `account_id` at each site (§9.3).
    #[cfg(any(test, feature = "test-support"))]
    pub fn accounts_mut(&mut self) -> &mut crate::account::AccountLedgers {
        &mut self.accounts
    }

    /// Mutable access to config — TEST SEAM ONLY (test cfg or feature
    /// `test-support`; #780/#825 boundary hygiene). Production writers go
    /// through narrow mutators (e.g. [`Self::set_tick_schedule_ms`]) or the
    /// runtime-config paths, so a naked config write can no longer bypass
    /// their validation from outside `service`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn config_mut(&mut self) -> &mut CoreConfig {
        &mut self.config
    }

    /// Point the exit ladder's deadline grid at the step the caller will
    /// actually drive (#390 replay nofast). `tick_schedule_ms` is the grid
    /// the counter ratchets backfill over (consumed by
    /// `check_exits_interval`), so a non-positive step would corrupt the
    /// schedule — normalized to 1ms, the same floor the replay driver used
    /// to apply at its call site before the naked `config_mut` write was
    /// closed (#780).
    pub fn set_tick_schedule_ms(&mut self, tick_ms: i64) {
        self.config.tick_schedule_ms = tick_ms.max(1);
    }

    /// Read-only view of the active config (diagnostics/backtest reports).
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    // ── Strategy engine & extensions (P0.5) ─────────────────────────────────
    /// Strategy names the kernel supports (the self-driving engine's set),
    /// deduplicated and sorted. Multi-account deployment hosts one scope per
    /// (strategy, account) pair, so the raw engine list repeats a strategy
    /// once per account — `strategy.list` and the stats surface are per-
    /// NAME tables and must see each name exactly once (PR H). Sorted, not
    /// run-length deduped: the second scope of a name registers AFTER every
    /// first-scope name, so duplicates are never adjacent.
    ///
    /// 策略全局可见（2026-10 操作者裁决）：the row set is the INSTALLED set
    /// (`package_sources` — sticky provenance, one entry per package LOADED
    /// this session, surviving scope teardowns) UNION the live scopes. Boot
    /// scans every package disabled, then the account enablement sets tear
    /// the unassigned scopes down — a registry read of live scopes alone
    /// would shrink the strategies page to the enabled few and lock every
    /// other strategy out of the panel's enable buttons (its assignable set
    /// is `strategy.list`). Torn-down names carry no live scope, so their
    /// row reads `accounts: []`, sizing/gate views fall back to the globals,
    /// and enabling one re-instantiates the scope from the sticky source.
    pub fn strategy_names(&self) -> Vec<String> {
        let mut names = self
            .engine
            .as_ref()
            .map(|e| e.supported_strategies())
            .unwrap_or_default();
        if let Some(e) = self.engine.as_ref() {
            for (name, _) in e.scope_sources() {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// Enabled strategy names.
    pub fn enabled_strategy_names(&self) -> Vec<String> {
        self.engine
            .as_ref()
            .map(|e| e.enabled_strategies())
            .unwrap_or_default()
    }

    /// Toggle a strategy on the driving engine. Returns false when the engine
    /// is not attached or the name is unknown.
    ///
    /// A toggle changes which strategies Shadow Evolution watches (it registers
    /// around the enabled set), so the manager is rewired immediately: switching a
    /// strategy on brings its parameters and variants in, switching it off drops
    /// them. No restart, and nothing is traded by a strategy that is off.
    ///
    /// 账户↔venue 解耦语义: this is the GLOBAL strategy switch (the master
    /// power). The per-account 启用按钮 rides
    /// [`Self::set_scoped_strategy_enabled`] below.
    ///
    /// PERSISTENCE ASYMMETRY (issue 552 audit, honest wording): this toggle is
    /// a SESSION-level fact on the default scope's enabled flag — it does not
    /// persist. The durable per-account truth is only the accounts.toml
    /// enablement set: a kernel restart replays it, so a scope named in an
    /// account's set boots ENABLED even if the session toggled the global
    /// switch off. To keep a strategy off across restarts, remove it from
    /// every account's set (the panel's 启用按钮 / `account.strategies.set`).
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let ok = self
            .engine
            .as_mut()
            .map(|e| e.set_strategy_enabled(name, enabled))
            .unwrap_or(false);
        if ok {
            self.rewire_hot_params(now_ms());
            // Record the operator's intent: the next boot replays this set.
            // `None` (backtests, most tests) writes nothing.
            if let Some(path) = &self.config.strategy_state_path {
                crate::strategy_state::save(
                    std::path::Path::new(path),
                    &self.enabled_strategy_names(),
                );
            }
        }
        ok
    }

    /// The per-account half of `strategy.enable {name, accountId?, reason?}`:
    /// set ONE (strategy, account) scope's enablement — persist to the
    /// account book's `strategies` set (memory first, disk after, one
    /// transaction) and drive the engine's scope state from it.
    ///
    /// Enable: the strategy joins the account's enablement set; a scope that
    /// does not exist yet is instantiated (Lua sources only — the same
    /// rebuild the boot uses) and arrives enabled; an existing
    /// exits-only-demoted scope returns to normal. Disable: the scope leaves
    /// the set; a flat scope is torn down, a scope holding positions is
    /// demoted EXITS-ONLY (no new entries, exits keep flowing to flat) —
    /// `kept_exits_only` names what was demoted, for the reply.
    ///
    /// `id` MUST be a configured account (the standard unknown-account
    /// refusal otherwise). `default` is a legal target: its scopes are the
    /// historical global-toggle shape, and its enablement set rides the same
    /// file. The GLOBAL toggle (`set_strategy_enabled`) is NOT touched —
    /// this is per-account state only.
    ///
    /// Returns `(kept_exits_only, set_now)` — the demoted names, and the
    /// account's enablement set as it now stands (the reply's echo).
    pub fn set_scoped_strategy_enabled(
        &mut self,
        id: &crate::account::AccountId,
        name: &str,
        enabled: bool,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> CoreResult<(Vec<String>, Vec<String>)> {
        use crate::schema::ACCOUNT_STRATEGIES_SET_NOTE;
        // Existence first, with the standard unknown-account refusal, before
        // anything touches disk.
        self.accounts.require(id)?;
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "strategy.enable: an empty strategy name cannot be addressed".to_string(),
            ));
        }
        // The assignable set is what `strategy.list` reports: every package
        // LOADED this session. Enabling an uninstalled strategy is refused —
        // a silent never-trading scope is the exact production accident the
        // rebuilt-scopes-start-ENABLED rule exists against.
        if self
            .engine
            .as_ref()
            .is_none_or(|e| !e.strategy_registered(trimmed))
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "strategy.enable: strategy `{trimmed}` is not registered — \
                     the assignable set is what `strategy.list` reports"
                ),
            ));
        }
        // The account's current enablement set (the book is the truth), then
        // the toggle.
        let mut set: Vec<String> = self
            .accounts
            .account(id)
            .map(|a| a.strategies.clone())
            .unwrap_or_default();
        let present = set.iter().any(|s| s == trimmed);
        if enabled && !present {
            set.push(trimmed.to_string());
        } else if !enabled {
            set.retain(|s| s != trimmed);
        }
        // Persist FIRST (a failure leaves both stores untouched — the same
        // memory-after-disk transaction every account writer runs; issue 751:
        // the durable half upserts ACCOUNT_* keys into .env).
        let Some((env_path, legacy)) = self.config.account_env_write_paths() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "strategy.enable: no accounts config path configured — persistence \
                 is disabled for this core (backtests and in-process tests); \
                 nothing was applied"
                    .to_string(),
            ));
        };
        crate::account::config::env_persist_account_strategies(
            &legacy,
            &env_path,
            id.as_str(),
            &set,
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "strategy.enable: persist failed, nothing changed: {}",
                    e.message
                ),
            )
        })?;
        self.accounts.set_strategies(id, set.clone(), now_ms)?;
        // #537: the default account's scoped write keeps the persisted
        // enabled-set in lockstep (the same mirror `set_account_strategies_
        // persisting` runs): the boot migrates the state file ONLY while the
        // default account has no `strategies` key, so a stale state file
        // would otherwise re-import a set the operator already emptied.
        if *id == crate::account::default_account_id()
            && let Some(state_path) = self.config.strategy_state_path.as_deref()
        {
            crate::strategy_state::save(std::path::Path::new(state_path), &set);
        }
        // Engine rebuild over the new set: removed-and-flat scopes drop,
        // removed-and-held scopes demote exits-only, added scopes
        // instantiate enabled.
        let held: Vec<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id == *id)
            .map(|p| p.strategy.clone())
            .collect();
        let kept_exits_only = self.rebuild_account_scopes(id, &set, &held);
        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %id,
            strategy = %trimmed,
            enabled,
            set = ?set,
            exits_only = ?kept_exits_only,
            reason = %reason,
            "per-account strategy enablement written — scope rebuilt from the \
             account's enablement set, re-read at next boot ({ACCOUNT_STRATEGIES_SET_NOTE})",
        );
        Ok((kept_exits_only, set))
    }

    /// The per-account enablement readout: the accounts whose enablement set
    /// names this strategy, sorted, plus the default scope when the global
    /// toggle is ON and default carries no key of its own (the zero-config
    /// shape has no enablement-set row to point at). This is the WRITE-path
    /// truth ("which accounts does this strategy trade on"), kept for the
    /// boot-replay teeth and any caller that wants enablement, not hosting.
    ///
    /// NOTE the deliberate difference from `Engine::strategy_scopes` — what
    /// `strategy.list`'s per-row `accounts` field serves (the coordination
    /// contract, wired in server.rs): the HOSTED scope list, exits-only
    /// demotions included. A scope pulled out of the enablement set while
    /// holding positions must keep showing on the matrix — the panel needs
    /// to see "this account still manages its exits here" — while this
    /// function answers the narrower "whose enablement button names it".
    pub fn strategy_enabled_accounts(&self, name: &str) -> Vec<String> {
        let mut accounts: Vec<String> = self
            .accounts
            .account_list()
            .into_iter()
            .filter(|a| a.strategies.iter().any(|s| s == name))
            .map(|a| a.id.as_str().to_string())
            .collect();
        // The zero-config shape: a `default` with no enablement key whose
        // global toggle is ON still runs the strategy — its row says so.
        let default_id = crate::account::default_account_id();
        let default_has_key = self
            .accounts
            .account(&default_id)
            .is_some_and(|a| !a.strategies.is_empty());
        if !default_has_key
            && self
                .engine
                .as_ref()
                .is_some_and(|e| e.enabled_strategies().iter().any(|n| n == name))
        {
            accounts.push(default_id.as_str().to_string());
        }
        accounts.sort();
        accounts.dedup();
        accounts
    }

    /// The handshake verdict for one declaration, as an associated function so
    /// the load path can run it while the engine borrow is live (only
    /// field-disjoint access is legal there). `None` = compatible: an
    /// undeclared strategy participates not (§8.1 — every 0.2 library's
    /// state), and no active plugin (backtester, tests) leaves nothing to
    /// refuse against.
    ///
    /// Multi-plugin: the declaration is judged against EVERY snapshot and the
    /// strategy loads when ANY ONE plugin can serve it; the refusal names each
    /// plugin's own mismatch so the diagnostic stays complete. Single-plugin
    /// books render exactly the historical message (no per-plugin prefix).
    pub(super) fn modes_refusal(
        plugins: &[crate::compat::PluginModes],
        name: &str,
        modes: &[blitzkrieg_strategy_api::StrategyMode],
    ) -> Option<String> {
        if modes.is_empty() {
            return None;
        }
        if plugins.is_empty() {
            return None;
        }
        let refusals: Vec<String> = plugins
            .iter()
            .filter_map(|p| crate::compat::check_compatibility(name, modes, p).err())
            .collect();
        if refusals.len() < plugins.len() {
            // At least one plugin passed (its refusal is absent): ANY one
            // satisfiable venue loads the strategy — a venue the strategy
            // cannot serve must not block the venues it can.
            return None;
        }
        // Every plugin refused: the diagnostic names each mismatch. ONE plugin
        // → the bare historical message, byte-identical; several → prefixed
        // and joined so the full mismatch surface is visible.
        if plugins.len() == 1 {
            return refusals.into_iter().next();
        }
        Some(format!(
            "incompatible modes: strategy '{name}' is not served by any active plugin — {}",
            refusals.join(" | ")
        ))
    }

    /// The handshake verdict for a strategy already in the dispatch, by name.
    pub(crate) fn compatibility_refusal(&self, name: &str) -> Option<String> {
        match self.strategy_declarations.get(name) {
            Some(modes) => Self::modes_refusal(&self.plugin_modes, name, modes),
            None => None,
        }
    }

    /// Inject every active plugin's declaration snapshot (server assembly,
    /// BEFORE any strategy is loaded or enabled, so all three §8.2 handshake
    /// sites judge against the same view). Empty leaves the handshake inert
    /// (backtester, tests).
    pub fn set_plugin_modes(&mut self, modes: Vec<crate::compat::PluginModes>) {
        self.plugin_modes = modes;
    }

    /// Test-only: record a declaration for a strategy the loader would have
    /// registered (the test adapters carry no C ABI declaration of their
    /// own). Same map the real load path fills.
    #[cfg(test)]
    pub(crate) fn set_strategy_declarations_for_test(
        &mut self,
        name: &str,
        modes: Vec<blitzkrieg_strategy_api::StrategyMode>,
    ) {
        self.strategy_declarations.insert(name.to_string(), modes);
    }

    /// The enable path with the handshake. Returns `(found, applied, reason)`:
    /// `found` says the name exists in the dispatch, `applied` says the toggle
    /// took effect, and `reason` carries the §8.2 diagnostic when the
    /// handshake refused an enable. Disabling is never gated — standing down
    /// is always allowed.
    pub fn set_strategy_enabled_checked(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> (bool, bool, Option<String>) {
        let found = self.strategy_names().iter().any(|n| n == name);
        if enabled
            && found
            && let Some(reason) = self.compatibility_refusal(name)
        {
            return (true, false, Some(reason));
        }
        let applied = self.set_strategy_enabled(name, enabled);
        (found, applied, None)
    }

    /// E27 (§8.3): the `strategy.list` view for one strategy — the declared
    /// modes as the §2.3 wire objects (`null` = undeclared), the compatible
    /// flag and the refusal reason when incompatible. Declarations describe
    /// live dispatch members only (recorded at load, cleared at unload).
    pub fn strategy_compat_view(&self, name: &str) -> (serde_json::Value, bool, Option<String>) {
        match self.strategy_declarations.get(name) {
            None => (serde_json::Value::Null, true, None),
            Some(modes) => {
                let wire: Vec<serde_json::Value> = modes
                    .iter()
                    .map(blitzkrieg_strategy_api::modes::mode_wire_json)
                    .collect();
                let refusal = Self::modes_refusal(&self.plugin_modes, name, modes);
                (serde_json::Value::Array(wire), refusal.is_none(), refusal)
            }
        }
    }

    /// `strategy.params.get`: one strategy's declared tunables plus the value
    /// in force. The schema source is the strategy's OWN knob declaration
    /// (manifest tunables for Lua packages) — the kernel invents no domain,
    /// and a strategy that declared nothing reports an empty list.
    pub fn strategy_params_readout(&self, name: &str) -> crate::schema::StrategyParamsReadout {
        use crate::schema::{StrategyParamsReadout, TunableView};
        let specs: Vec<_> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_refs()
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.evolvable_knobs())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let found = self.strategy_names().iter().any(|n| n == name);
        // The current value in force: the registry cell when one exists, else
        // the declared value (the same thing while no override ever landed).
        let mut tunables: Vec<TunableView> = specs
            .iter()
            .map(|k| {
                let value = self
                    .shadow_evolution
                    .params_for(name)
                    .and_then(|p| p.get(&k.name))
                    .unwrap_or(k.value);
                TunableView {
                    name: k.name.clone(),
                    // All in-tree declarations are decimal-typed; the `int`
                    // tag only exists on the Lua manifest surface, which the
                    // set path re-checks against the declared domain. Expose
                    // the knob names the kernel actually enforces.
                    r#type: None,
                    value: value.to_string(),
                    min: k.min.to_string(),
                    max: k.max.to_string(),
                }
            })
            .collect();
        tunables.sort_by(|a, b| a.name.cmp(&b.name));
        let evolvable = !specs.is_empty();
        let params_attached = self.has_hot_params();
        let note = if !found {
            Some(format!("strategy {name} is not loaded"))
        } else if !evolvable {
            Some("该策略没有可调参数（manifest 未声明 tunables 域）".to_string())
        } else {
            None
        };
        StrategyParamsReadout {
            name: name.to_string(),
            found,
            tunables,
            evolvable,
            params_attached,
            note,
        }
    }

    /// `strategy.params.set` (issue #481 G3): the tunables' operator write
    /// path — the `risk.setExit` posture (plan first, write later, audit the
    /// receipt). Validation is against the strategy's OWN declared domains;
    /// a refused patch moves nothing.
    ///
    /// 生效路径（本轮裁决）：写入 = `registry.publish`（该策略 cell 的新值）
    /// 加引擎侧 attach。进化关闭时 `rewire_hot_params` 会把覆盖层脱钩，所以
    /// 一次操作员写入要生效必须把覆盖层重新挂上。这与「关闭进化 = 行为与
    /// 改动前一致」的验收共存：脱钩语义是「没有写入时等于没有覆盖层」，而
    /// 一旦操作员显式写了 tunables，挂载覆盖层就是那个写入的直接后果 ——
    /// 下次 evaluate 起生效；重启后覆盖层消失，回落 manifest 默认（回执
    /// note 明说 `persisted: false`）。进化的梯度锁（±5%）刻意不用于此路径：
    /// 操作员编辑不是进化的一个变体步，只有声明域是硬边界。
    pub fn apply_strategy_params_set(
        &mut self,
        name: &str,
        knobs: &std::collections::BTreeMap<String, serde_json::Value>,
        actor: &str,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<crate::schema::StrategyParamsSetUpdate, crate::model::CoreError> {
        use crate::model::{CoreError, CoreErrorCode};
        use crate::schema::StrategyParamChange;
        use strategy_logic::params::StrategyParams as KnobBag;

        let err = |msg: String| CoreError::new(CoreErrorCode::InvalidParams, msg);
        // The declaration lives on the live instance (E2-c); a strategy that
        // is not hosted has nothing to validate against and nothing to write.
        let specs = self
            .engine
            .as_ref()
            .and_then(|e| {
                e.strategy_refs()
                    .iter()
                    .find(|s| s.name() == name)
                    .map(|s| s.evolvable_knobs())
            })
            .unwrap_or_default();
        if specs.is_empty() {
            return Err(err(format!(
                "strategy {name} declares no tunables (nothing is writable); \
                 read the declared domains with strategy.params.get"
            )));
        }
        if knobs.is_empty() {
            return Err(err(
                "strategy.params.set needs at least one knob; read the declared \
                 domains with strategy.params.get"
                    .into(),
            ));
        }

        // Plan first: every knob must be declared, parse exactly (decimal
        // string or JSON number — no float path) and sit inside its domain.
        let mut plan: Vec<(String, Decimal)> = Vec::new();
        for (knob, raw) in knobs {
            let Some(spec) = specs.iter().find(|s| &s.name == knob) else {
                return Err(err(format!(
                    "knob {knob} is not declared by {name} — nothing was applied; \
                     the declared tunables are: {}",
                    specs
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            };
            let value = match raw {
                serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()),
                serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()),
                other => {
                    return Err(err(format!(
                        "knob {knob}: expected a decimal string, got {other}"
                    )));
                }
            };
            let Ok(value) = value else {
                return Err(err(format!(
                    "knob {knob}: {raw} is not an exact decimal — nothing was applied"
                )));
            };
            if !spec.contains(value) {
                return Err(err(format!(
                    "knob {knob}: {value} is outside the declared domain [{}, {}] — \
                     nothing was applied",
                    spec.min, spec.max
                )));
            }
            plan.push((knob.clone(), value));
        }

        // Write phase: merge the patch over the cell's current values (a
        // single-knob patch keeps the strategy's other knobs in force), then
        // publish + attach so the strategy reads it on the next evaluate.
        let registry = self.shadow_evolution.registry();
        let mut next: KnobBag = registry
            .handle_for(name)
            .map(|c| (**c.load()).clone())
            .unwrap_or_else(|| KnobBag::from_knobs(&specs));
        let mut changes: Vec<StrategyParamChange> = Vec::new();
        for (knob, value) in &plan {
            let from = next.get(knob).unwrap_or(Decimal::ZERO).to_string();
            next.set(knob, *value);
            changes.push(StrategyParamChange {
                knob: knob.clone(),
                from,
                to: value.to_string(),
                effect: "live",
            });
        }
        registry.publish(name, next);
        // Attach the overlay to the engine (see the method doc for why this is
        // unconditional): the set path IS an overlay write.
        if let Some(e) = self.engine.as_mut() {
            e.set_hot_params(Some(std::sync::Arc::clone(&registry)));
        }

        for c in &changes {
            tracing::info!(
                target: "strategy_params",
                actor = %actor,
                strategy = %name,
                knob = c.knob,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = reason.as_deref().unwrap_or(""),
                "strategy tunables update: {}.{} {} -> {}",
                name,
                c.knob,
                c.from,
                c.to,
            );
        }
        // The evolution audit log carries manual overrides; this write is one
        // (same shape `shadow_evolution_apply` records), so an operator edit
        // shows up next to every adopted promotion.
        let before = {
            let mut bag = KnobBag::from_knobs(&specs);
            // from-values as recorded in the change list (the pre-write cell
            // read raced nothing: both reads happened inside this lock).
            for c in &changes {
                if let Ok(v) = Decimal::from_str_exact(&c.from) {
                    bag.set(&c.knob, v);
                }
            }
            let mut m = crate::shadow_evolution::knobs::MutableParams::new();
            m.set_strategy(name, bag);
            m
        };
        let after = {
            let mut m = crate::shadow_evolution::knobs::MutableParams::new();
            if let Some(p) = registry.handle_for(name).map(|c| (**c.load()).clone()) {
                m.set_strategy(name, p);
            }
            m
        };
        self.shadow_evolution.audit_manual_override(
            name,
            now_ms,
            &before,
            &after,
            "operator tunables edit",
        );

        Ok(crate::schema::StrategyParamsSetUpdate {
            name: name.to_string(),
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason,
            persisted: false,
            note: crate::schema::STRATEGY_PARAMS_SET_NOTE,
        })
    }

    /// `unified.list` (issue #481 G4): the confirmed cross-venue mappings as a
    /// read-only snapshot — per event: status, the venues holding a leg, and
    /// the discrepancy marks. Data only; the panel renders venues as rows, no
    /// venue branch anywhere.
    pub fn unified_list(&self) -> Vec<serde_json::Value> {
        self.engine
            .as_ref()
            .map(|e| {
                e.unified_mappings()
                    .values()
                    .map(|ev| {
                        serde_json::json!({
                            "id": ev.id,
                            "title": ev.title,
                            "status": ev.status,
                            "settlementSource": ev.settlement_source,
                            "venues": ev
                                .listings
                                .values()
                                .map(|l| serde_json::json!({
                                    "venue": l.venue,
                                    "conditionId": l.condition_id,
                                    "asset": l.asset,
                                    "expiresAtMs": l.expires_at_ms,
                                    "status": l.status,
                                }))
                                .collect::<Vec<_>>(),
                            "discrepancies": ev
                                .discrepancies
                                .iter()
                                .map(|d| d.as_str())
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// ④ 统一映射：一轮 RoundMarkets 到达后，把这轮每个市场转成
    /// [`PlatformListing`] 喂给映射账本的候选匹配器（只提议、绝不自动
    /// confirm —— ledger 的合同是 "automatic matching proposes, the operator
    /// disposes"，自动配对会违反 cross_venue_replay_check 的 0 订单负向门）。
    /// settlement 字段发现链路拿不到：空 rules + Unknown source（双空规则在
    /// matcher 里不捏造冲突）。结果存 `unified_candidates` 供
    /// `unified.candidates` 读；每个新轮全量替换（旧轮的过期候选不留）。
    pub(super) fn propose_unified_candidates(
        &mut self,
        markets: &[crate::model::CryptoMarket],
        now_ms: i64,
    ) {
        use blitzkrieg_market_api::unified::{
            ListingStatus, PlatformListing, SettlementSource, Venue,
        };
        self.unified_candidates.clear();
        let venue_of = |s: &str| match s {
            "polymarket" => Some(Venue::Polymarket),
            "kalshi" => Some(Venue::Kalshi),
            "predictfun" => Some(Venue::PredictFun),
            "polysimulator" => Some(Venue::PolySimulator),
            _ => None, // undeclared venue (old archives): nothing to pair
        };
        let mut by_venue: std::collections::BTreeMap<Venue, Vec<PlatformListing>> =
            Default::default();
        for m in markets {
            let Some(v) = venue_of(m.venue.as_str()) else {
                continue;
            };
            by_venue.entry(v).or_default().push(PlatformListing {
                venue: v,
                condition_id: m.condition_id.clone(),
                up_token_id: m.up_token_id.clone(),
                down_token_id: m.down_token_id.clone(),
                // matcher 的资产信号是归一化小写（"bitcoin"）；发现链路给的是 "BTC"。
                asset: Some(m.asset.to_lowercase()),
                title: m.question.clone(),
                settlement_rules: String::new(),
                settlement_source: SettlementSource::Unknown,
                expires_at_ms: m.expires_at_ms,
                status: if m.expires_at_ms > now_ms {
                    ListingStatus::Active
                } else {
                    ListingStatus::Expired
                },
            });
        }
        if by_venue.len() < 2 {
            return;
        }
        let buckets: Vec<Vec<PlatformListing>> = by_venue.into_values().collect();
        for i in 0..buckets.len() {
            for j in (i + 1)..buckets.len() {
                for cand in self
                    .unified_ledger
                    .candidates(&buckets[i], &buckets[j], 10 * 60 * 1000)
                {
                    tracing::info!(
                        target: "risk",
                        score = cand.score,
                        a_venue = cand.a.venue.as_str(),
                        b_venue = cand.b.venue.as_str(),
                        title_a = %cand.a.title,
                        title_b = %cand.b.title,
                        "unified: cross-venue mapping candidate proposed — confirm via unified.confirm"
                    );
                    self.unified_candidates.push(cand);
                }
            }
        }
    }

    /// `unified.candidates` (④): the latest proposal snapshot as read-only
    /// JSON. Empty when the current round has fewer than two declared venues.
    pub fn unified_candidates(&self) -> Vec<serde_json::Value> {
        self.unified_candidates
            .iter()
            .map(|c| {
                serde_json::json!({
                    "a": listing_json(&c.a),
                    "b": listing_json(&c.b),
                    "score": c.score,
                    "expiryDeltaMs": c.expiry_delta_ms,
                })
            })
            .collect()
    }

    /// `unified.confirm` (④): the operator confirms a candidate pair as a
    /// cross-venue mapping. The ledger revalidates (title similarity, asset
    /// identity, one-listing-one-event) and persists; the confirmed event is
    /// pushed into the Engine so strategies see it immediately, and the
    /// confirmed listings leave the proposal shelf.
    pub fn unified_confirm(
        &mut self,
        title: &str,
        actor: &str,
        listings: Vec<blitzkrieg_market_api::unified::PlatformListing>,
        at_ms: i64,
    ) -> Result<blitzkrieg_market_api::unified::UnifiedEvent, String> {
        let event = self
            .unified_ledger
            .confirm(title, actor, &listings, at_ms)
            .map_err(confirm_error_text)?;
        let confirmed_ids: std::collections::HashSet<&str> = event
            .listings
            .values()
            .map(|l| l.condition_id.as_str())
            .collect();
        self.unified_candidates.retain(|c| {
            !confirmed_ids.contains(c.a.condition_id.as_str())
                && !confirmed_ids.contains(c.b.condition_id.as_str())
        });
        if let Some(e) = self.engine.as_mut() {
            e.put_unified_mapping(event.clone());
        }
        Ok(event)
    }

    /// `unified.revoke` (④): withdraw a confirmed mapping. The ledger drops
    /// the event (audited, persisted); the Engine's copy is removed here too
    /// — the Engine has only `put` semantics, so revocation is the host's
    /// job.
    pub fn unified_revoke(
        &mut self,
        event_id: &str,
        actor: &str,
        at_ms: i64,
    ) -> Result<(), String> {
        self.unified_ledger
            .revoke(event_id, actor, at_ms)
            .map_err(|e| format!("unified revoke refused: {e:?}"))?;
        if let Some(e) = self.engine.as_mut() {
            e.remove_unified_mapping(event_id);
        }
        Ok(())
    }

    /// Load an external v2 strategy shared library and register it into the
    /// driving engine's live dispatch (the IPC `strategy.load` entry). Runs
    /// the E27 handshake in ENFORCE mode: a library whose declaration the
    /// active plugin cannot serve is refused, not registered-and-unable-to-
    /// enable (§8.2 row 2).
    pub fn load_strategy_lib(&mut self, path: &str) -> String {
        self.load_strategy_lib_inner(path, true)
    }

    /// The startup scan path (`load_strategy_dir`). §8.2 row 1: the scan only
    /// LOGS what was declared — it never refuses. An incompatible library
    /// still registers (disabled) with the mismatch named on its receipt;
    /// the enable handshake (both `strategy.enable` and the startup
    /// `--enable-strategy` loop) is the gate.
    pub(super) fn load_strategy_lib_scan(&mut self, path: &str) -> String {
        self.load_strategy_lib_inner(path, false)
    }

    pub(super) fn load_strategy_lib_inner(&mut self, path: &str, enforce: bool) -> String {
        #[cfg(feature = "strategy-loading")]
        {
            use crate::strategy_engine::loader::load_foreign;
            let p = std::path::Path::new(path);
            // E30 (§6.4): a DIRECTORY is a Lua package, never a dylib — route
            // it through the Lua loader so the two stacks cannot load each
            // other's artifacts (the dylib scanner owns `*.dylib`, the Lua
            // loader owns `manifest.json` packages).
            if p.is_dir() {
                return self.load_lua_package_inner(p, enforce);
            }
            let Some(engine) = self.engine.as_mut() else {
                return "Failed: strategy engine not attached (load libraries after engine init)"
                    .into();
            };
            match load_foreign(p) {
                Ok(loaded) => {
                    let name = loaded.name.clone();
                    let version = loaded.version.clone();
                    // E27 (§8.2): the handshake verdict BEFORE registration.
                    // ENFORCE (the IPC `strategy.load` path): a library whose
                    // declaration the active plugin cannot serve is refused,
                    // not registered-and-silently-unable-to-enable. SCAN (the
                    // startup `strategy_dir` walk): §8.2 row 1 — the scan only
                    // logs, so the library registers disabled and the receipt
                    // itself names the mismatch; the enable handshake is the
                    // gate there. Either way the text names both sides (R-B).
                    let refusal =
                        Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                    if enforce && let Some(refusal) = refusal {
                        // `refusal` already opens with "incompatible modes:" and
                        // names both sides (§8.2) — prefix only the verdict tag.
                        return format!("Failed: {refusal}");
                    }
                    let incompatible = match refusal {
                        Some(r) => format!("; INCOMPATIBLE: {r}"),
                        None => String::new(),
                    };
                    // E2-b: the load receipt names the gates the library declared
                    // unnecessary, so an opt-out is visible before it is enabled.
                    // D-31: if the declaration carries a `time_left_sec` floor,
                    // state it — the receipt is the operator's only pre-enable
                    // view of how far the opt-out actually reaches.
                    let declared = if loaded.gate_exemptions.any() {
                        let floor = match loaded.gate_exemptions.timing_min_time_left_sec {
                            Some(n) => format!(" (timing floor {n}s)"),
                            None => String::new(),
                        };
                        format!(
                            "; declares gate exemptions: {}{floor}",
                            loaded.gate_exemptions.gates().join(",")
                        )
                    } else {
                        String::new()
                    };
                    // E2-c: the same receipt names the evolvable knobs, so "not
                    // evolvable" (no optional symbol) is distinguishable at load
                    // from "evolvable but nothing applied yet".
                    let evolvable = if loaded.evolvable_knobs.is_empty() {
                        "; not evolvable (no knobs declared)".to_string()
                    } else {
                        let names: Vec<&str> = loaded
                            .evolvable_knobs
                            .iter()
                            .map(|k| k.name.as_str())
                            .collect();
                        format!("; declares evolvable knobs: {}", names.join(","))
                    };
                    match engine.register_user_strategy(
                        Box::new(loaded.strategy),
                        format!("dylib:{}", p.display()),
                    ) {
                        Ok(_) => {
                            // E27: the declaration is recorded only on a
                            // successful registration, so `strategy.list` and
                            // the enable-time handshake describe live dispatch
                            // members and nothing else.
                            self.strategy_declarations
                                .insert(name.clone(), loaded.declared_modes.clone());
                            // The new strategy's declaration must reach the manager
                            // too: a library loaded AFTER evolution was enabled would
                            // otherwise never get a unit, so it could not evolve at
                            // all until a restart.
                            self.rewire_hot_params(now_ms());
                            // API 1.0 naming (DEV_V0_3 §2.2, E24 #330): the
                            // receipt KEEPS its old text — the test above pins
                            // it — and only APPENDS the protocol line. E24
                            // handed this hunk to the service.rs hotspot
                            // guardian (E25); it lands here.
                            format!(
                                "{name}@{version} registered into the engine dispatch (disabled{declared}{evolvable}{incompatible}); API 1.0 (line protocol 2)"
                            )
                        }
                        Err(reason) => format!("rejected: {reason}"),
                    }
                }
                Err(outcome) => format!("{outcome:?}"),
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let outcome = crate::strategy_engine::loader::load_strategy(std::path::Path::new(path));
            format!("{outcome:?}")
        }
    }

    /// E30 (#336): load one Lua strategy package (a directory carrying
    /// `manifest.json`) and register it into the live dispatch. The E27
    /// handshake applies UNCHANGED — a Lua declaration the active plugin
    /// cannot serve is refused in ENFORCE mode (IPC `strategy.load`) or
    /// registered-disabled-with-verbatim-receipt in SCAN mode (the startup
    /// walk), byte-identical to the dylib rows (§8.2). The receipt carries
    /// the `(lua)` marker so an operator sees the runtime at a glance
    /// (§6.6: 回执含 `(lua)` 标记). A poisoning breach inside the sandbox is
    /// reported on the receipt; it is not a load failure (the strategy is
    /// alive, it just refused to run).
    pub(super) fn load_lua_package_inner(
        &mut self,
        dir: &std::path::Path,
        enforce: bool,
    ) -> String {
        use crate::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
        let Some(engine) = self.engine.as_mut() else {
            return "Failed: strategy engine not attached (load libraries after engine init)"
                .into();
        };
        match load_lua_package(dir) {
            Ok(loaded) => {
                let name = loaded.name.clone();
                let version = loaded.version.clone();
                let tunables = loaded.tunables.clone();
                let holds = loaded.holds_to_settlement;
                let exemptions = loaded.gate_exemptions;
                let atomic = loaded.atomic_batch;
                let trailing = loaded.trailing_exit;
                let entry_maker = loaded.entry_maker;
                // E27 (§8.2): the SAME handshake verdict BEFORE registration.
                let refusal =
                    Self::modes_refusal(&self.plugin_modes, &name, &loaded.declared_modes);
                if enforce && let Some(refusal) = refusal {
                    return format!("Failed: {refusal}");
                }
                let incompatible = match refusal {
                    Some(r) => format!("; INCOMPATIBLE: {r}"),
                    None => String::new(),
                };
                // The fee projection (`bk.fees()`): the kernel's ONE schedule,
                // stamped at load so a strategy prices the fee from the same
                // curve the charge path settles in.
                let sched = crate::exit_policy::fee_schedule();
                let fee_view = blitzkrieg_lua_runtime::FeeScheduleView {
                    name: sched.name.to_string(),
                    rate: sched.rate.to_string(),
                    exponent: sched.exponent,
                };
                let adapter = LuaEngineAdapter::new(loaded.strategy, tunables)
                    .declare(holds, exemptions)
                    .declare_atomic_batch(atomic)
                    .declare_trailing_exit(trailing)
                    .declare_entry_maker(entry_maker)
                    // RFC-645 A-1: the ONE arbiter package declares the
                    // pair-book statistics surface (决议②: the loader answers
                    // for the Lua side); every other package keeps the
                    // `false` default. The engine's acceptance predicate
                    // still gates the row's arb* fields.
                    .declare_pair_book_stats(name == ARB_STRATEGY);
                adapter.set_fee_schedule(fee_view);
                match engine
                    .register_user_strategy(Box::new(adapter), format!("lua:{}", dir.display()))
                {
                    Ok(_) => {
                        // E30: an ABSENT manifest.modes stays absent — an empty
                        // Vec would render `modes: []` on strategy.list, and
                        // §8.3's contract is `null = undeclared` (E27). Insert
                        // only a package that actually declared modes.
                        if !loaded.declared_modes.is_empty() {
                            self.strategy_declarations
                                .insert(name.clone(), loaded.declared_modes.clone());
                        }
                        self.rewire_hot_params(now_ms());
                        format!(
                            "{name}@{version} (lua) registered into the engine dispatch (disabled{incompatible})"
                        )
                    }
                    Err(reason) => format!("rejected: {reason}"),
                }
            }
            Err(reason) => format!("Failed: {reason}"),
        }
    }

    /// Unload a dynamic strategy library (E9-b): drop its instance from the
    /// live dispatch and `dlclose` it. The engine refuses in-tree or still-
    /// enabled strategies; here we additionally refuse an OPEN POSITION held
    /// under this strategy's name — a strategy that still owns exposure is not
    /// removed from under the exit machinery. Auditable by design: the outcome
    /// string is what the operator sees on the IPC receipt.
    pub fn unload_strategy(&mut self, name: &str) -> String {
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — close them before unloading"
            );
        }
        match self
            .engine
            .as_mut()
            .map(|e| e.unregister_user_strategy(name))
        {
            Some(Ok(true)) => {
                // E27: the declaration leaves with the instance — the
                // handshake and `strategy.list` describe live members only.
                self.strategy_declarations.remove(name);
                format!("{name} unloaded (engine dispatch removed); library freed")
            }
            Some(Ok(false)) => format!("not found: no strategy named {name}"),
            Some(Err(reason)) => format!("rejected: {reason}"),
            None => "Failed: strategy engine not attached".into(),
        }
    }

    /// Load + swap in one call (E9-b `strategy.reload`): load the NEW library
    /// file first, and only when it parses and registers the SAME name does
    /// the old instance drop. A reload failure leaves the old instance live —
    /// a broken build can never blank a running dispatch. Because the swap is
    /// name-keyed, after-reload enable state is preserved.
    pub fn reload_strategy(&mut self, name: &str, path: &str) -> String {
        // 1. The old one must be removable-by-name (enabled/in-tree/open-pos
        //    guards all run below, BEFORE the new instance replaces it).
        let source = self
            .engine
            .as_ref()
            .map(|e| e.strategy_source(name).map(|s| s.to_string()))
            .unwrap_or(None);
        let Some(old_source) = source else {
            return format!("not found: no strategy named {name}");
        };
        let was_enabled = self.enabled_names_contains(name);
        // Temporary disable is NOT needed — the swap runs below; but a still-
        // enabled strategy cannot be swapped by the engine's guard, so the
        // load is tried first and the guard happens after.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — a reload with exposure in flight is undefined; close first"
            );
        }

        // 2. Load the new instance in a SCRATCH seat (a temp name would confuse
        //    profiles; instead we stage the strategy outside the engine only if
        //    the name differs). The kernel's loader gives us the Received view;
        //    both old and new live in the dispatch under the SAME name below.
        let old = self.unload_strategy(name);
        if !old.starts_with(name) && !old.contains("unloaded") {
            // rejected/not-found/Failed — refuse before loading new
            return format!("reload aborted: {old}");
        }
        let receipt = self.load_strategy_lib(path);
        if !receipt.contains("registered") {
            return format!("reload FAILED (old instance already dropped): {receipt}");
        }
        // Preserve previous enable state.
        if was_enabled {
            self.set_strategy_enabled(name, true);
        }
        let _ = old_source; // receipt text already carries the new source
        format!("reload OK: {receipt}")
    }

    pub(super) fn enabled_names_contains(&self, name: &str) -> bool {
        self.enabled_strategy_names().contains(&name.to_string())
    }

    /// List installed extensions (name, type, lifecycle state).
    pub fn extension_list(
        &self,
    ) -> Vec<(
        String,
        crate::extension::ExtensionType,
        crate::extension::ExtensionState,
    )> {
        self.extensions.list()
    }

    /// All-time closed-trade summary for the UI (totalTrades / wins / losses /
    /// totalNetPnl / …). Prefers the persisted summary tracked by the trade db;
    /// falls back to totals recomputed over the JSONL (or in-memory history)
    /// when no db is attached.
    ///
    /// `account = None` is the historical all-accounts call, byte-identical to
    /// pre-H behaviour. `account = Some(id)` recomputes the SAME aggregation
    /// over only that account's rows: the persisted summary is a GLOBAL
    /// accumulator (it cannot answer "this account alone"), so an account query
    /// always folds rows — the persisted file stays the all-accounts headline.
    /// Rows predating E28 carry no accountId and read as the default account,
    /// so an account query on a legacy log still sees them.
    pub fn trade_summary(&self, account: Option<&str>) -> serde_json::Value {
        if account.is_none()
            && let Some(db) = self.trade_db.as_ref()
        {
            return serde_json::to_value(db.summary()).unwrap_or(serde_json::Value::Null);
        }
        // Fallback / per-account path: aggregate over whatever history we can
        // reach, filtered to the requested account when one was named. The
        // account comparison goes through the record's serde-defaulted
        // accountId: empty on pre-E28 rows = the default account.
        let want = account.unwrap_or(crate::account::DEFAULT_ACCOUNT_ID);
        let empty = Vec::new();
        let all_rows = self.recent_trades(0);
        let rows: Vec<serde_json::Value> = match account {
            None => all_rows,
            Some(_) => all_rows
                .into_iter()
                .filter(|r| {
                    let rid = r
                        .get("accountId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    // An unattributed (pre-E28) row belongs to the default
                    // account; every other row matches by exact id.
                    if rid.is_empty() {
                        want == crate::account::DEFAULT_ACCOUNT_ID
                    } else {
                        rid == want
                    }
                })
                .collect(),
        };
        let rows = if rows.is_empty() { &empty } else { &rows };
        let mut s = serde_json::json!({
            "totalTrades": 0u64, "wins": 0u64, "losses": 0u64, "winRate": 0.0,
            "totalGrossPnl": 0.0, "totalFees": 0.0, "totalNetPnl": 0.0,
            "avgHoldTimeSec": 0.0, "bestTradePnl": 0.0, "worstTradePnl": 0.0,
        });
        let (mut wins, mut losses, mut gross, mut fees, mut net, mut best, mut worst, mut holds) = (
            0u64,
            0u64,
            0.0,
            0.0,
            0.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            0.0,
        );
        for r in rows.iter() {
            let num = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let net_i = num("netPnlUsd");
            if net_i >= 0.0 {
                wins += 1
            } else {
                losses += 1
            }
            gross += num("grossPnlUsd");
            fees += num("feesUsd");
            net += net_i;
            best = best.max(net_i);
            worst = worst.min(net_i);
            holds += num("holdTimeSec");
        }
        let n = (wins + losses) as f64;
        s["totalTrades"] = serde_json::json!(wins + losses);
        s["wins"] = serde_json::json!(wins);
        s["losses"] = serde_json::json!(losses);
        s["winRate"] = serde_json::json!(if n > 0.0 {
            wins as f64 * 100.0 / n
        } else {
            0.0
        });
        s["totalGrossPnl"] = serde_json::json!(gross);
        s["totalFees"] = serde_json::json!(fees);
        s["totalNetPnl"] = serde_json::json!(net);
        s["avgHoldTimeSec"] = serde_json::json!(if n > 0.0 { holds / n } else { 0.0 });
        s["bestTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { best });
        s["worstTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { worst });
        s
    }

    /// Recent closed trades (for the UI). Reads the persisted log when present,
    /// else falls back to the in-memory closed positions.
    pub fn recent_trades(&self, limit: usize) -> Vec<serde_json::Value> {
        if let Some(path) = self.config.trade_log_path.as_ref() {
            let rows = crate::trade_db::read_recent(std::path::Path::new(path), limit);
            if !rows.is_empty() {
                return rows;
            }
        }
        let closed = self.positions.closed_positions();
        let start = if limit > 0 && closed.len() > limit {
            closed.len() - limit
        } else {
            0
        };
        closed[start..]
            .iter()
            .map(|c| {
                serde_json::to_value(crate::trade_db::TradeRecord::from_closed(c))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect()
    }

    // ── Shadow Evolution (opt-in) ───────────────────────────────────────────
    pub fn shadow_evolution(&self) -> &ShadowEvolution {
        &self.shadow_evolution
    }

    /// Operator override for ONE strategy: validate + hot-swap + audit. `params`
    /// must name exactly one strategy; a multi-strategy bag is refused rather than
    /// partially applied.
    pub fn shadow_evolution_apply(
        &mut self,
        params: MutableParams,
        now_ms: i64,
    ) -> Result<(), String> {
        self.shadow_evolution.apply_params(params, now_ms)
    }

    pub fn shadow_evolution_enable(&mut self, now_ms: i64) -> bool {
        self.shadow_evolution.enable(now_ms);
        self.rewire_hot_params(now_ms);
        true
    }

    pub fn shadow_evolution_disable(&mut self) -> bool {
        self.shadow_evolution.disable();
        self.rewire_hot_params(now_ms());
        true
    }

    /// Aggregate status across every evolvable strategy (kept for callers that
    /// predate per-strategy status). `shadow_evolution_status_for` is the precise
    /// per-strategy query.
    pub fn shadow_evolution_status(&self, now_ms: i64) -> EvolutionStatus {
        self.shadow_evolution.aggregate_status(now_ms)
    }

    /// One strategy's status (`None` = not evolvable).
    pub fn shadow_evolution_status_for(
        &self,
        strategy: &str,
        now_ms: i64,
    ) -> Option<EvolutionStatus> {
        self.shadow_evolution.status(strategy, now_ms)
    }

    /// Roll back ONE strategy to the parameters in force before its last change.
    pub fn shadow_evolution_rollback(
        &mut self,
        strategy: &str,
        now_ms: i64,
    ) -> Result<EvolutionOutcome, String> {
        self.shadow_evolution.rollback(strategy, now_ms)
    }

    pub fn shadow_evolution_variants(
        &mut self,
        now_ms: i64,
    ) -> Vec<crate::shadow_evolution::VariantView> {
        self.shadow_evolution.variant_views(now_ms)
    }

    /// Audit history. `None` = every strategy; `Some(name)` = that strategy's file.
    pub fn shadow_evolution_history(
        &self,
        strategy: Option<&str>,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::audit::AuditRecord> {
        self.shadow_evolution.history(strategy, limit)
    }

    /// E13: every known proposal's latest state, newest first (IPC surface).
    pub fn shadow_evolution_proposals(
        &self,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::EvolutionProposal> {
        self.shadow_evolution.all_proposals(limit)
    }

    /// E13: the operator's verdict on one held proposal. String is the IPC-side
    /// verb ("accept"/"reject"/"defer"); anything else is refused loudly.
    pub fn shadow_evolution_decide(
        &mut self,
        id: &str,
        decision: &str,
        now_ms: i64,
    ) -> Result<crate::shadow_evolution::DecisionResult, String> {
        let decision = match decision {
            "accept" | "accepted" => crate::shadow_evolution::Decision::Accept,
            "reject" | "rejected" => crate::shadow_evolution::Decision::Reject,
            "defer" | "deferred" => crate::shadow_evolution::Decision::Defer,
            other => return Err(format!("unknown decision '{other}' (accept|reject|defer)")),
        };
        self.shadow_evolution.decide(id, decision, now_ms)
    }

    /// E13: the auto-evolve switch (the UIs' checkbox). Persisted across restarts.
    pub fn shadow_evolution_set_auto(&mut self, on: bool) -> bool {
        self.shadow_evolution.set_auto_evolve(on);
        self.shadow_evolution.auto_evolve()
    }

    /// E13: the 72h deep-evolution clock. Fires a compound-mutation round when
    /// the interval has elapsed; the returned event, if any, is emitted.
    pub fn shadow_evolution_maybe_cycle(&mut self, now_ms: i64) {
        if let Some(ev) = self.shadow_evolution.maybe_evolution_cycle(now_ms) {
            self.emit(Event::EvolutionCycle {
                cycle_seq: ev.cycle_seq,
                dims: ev.dims,
                strategies: ev.strategies,
                at_ms: ev.at_ms,
            });
        }
    }

    /// Feed the evolution engine a round's markets plus the opening book seeds the
    /// live engine is about to replay, so every twin's confirmation clock starts on
    /// the same tick as the live strategy's.
    pub fn shadow_evolution_on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, crate::model::OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.shadow_evolution.on_round(markets, seeds, now_ms);
    }

    /// Feed the evolution engine a book tick (observation only). Twins derive
    /// their own trend state from the books they see, exactly as an external
    /// strategy does, so no externally-computed confirmation flag is passed in.
    pub fn shadow_evolution_on_tick(
        &mut self,
        token_id: &str,
        book: &crate::model::OrderbookSnapshot,
        now_ms: i64,
    ) {
        self.shadow_evolution.on_tick(token_id, book, now_ms);
    }

    /// Run one evolution evaluation and map outcomes to kernel events.
    ///
    /// Called every tick regardless of the engine switch, because the pass is not
    /// only "evaluate": it also expires undecided proposals past their TTL, which
    /// is bookkeeping the operator's "7 天未决自动过期" is a promise about. The
    /// switch itself is the manager's own gate (#249) — one place, so a disabled
    /// engine can neither evaluate nor adopt, and cannot silently stop ageing the
    /// queue it left behind either.
    pub fn shadow_evolution_evaluate(&mut self, now_ms: i64) {
        for outcome in self.shadow_evolution.evaluate(now_ms) {
            self.emit_shadow_outcome(outcome);
        }
    }
}
