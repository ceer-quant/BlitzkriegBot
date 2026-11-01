//! Core assembly: the constructors, boot-time account/policy loading, the
//! execution-policy surface (IPC + reload) and the event sink. Pure code
//! motion from `service.rs` (#780) — no logic edits.

use super::*;

impl Core {
    pub fn new(config: CoreConfig) -> Self {
        // E28 (§9.5) + issue 751 (.env single source): the account book loads
        // through the ACCOUNT_* env grammar first (process env, then the
        // `.env` file), falling back to the legacy `user_layer/configs/
        // accounts.toml`, falling back to one `default` account. A malformed
        // source refuses the boot (fail-closed: a book that is not what the
        // operator wrote must not trade). The credential probe runs here too,
        // so `account.list` answers `credentialsLoaded` from boot-time facts.
        // Tests (and embedders) inject a book directly via
        // [`Core::with_account_book`] instead of touching the cwd.
        let accounts = crate::account::config::load_account_ledgers_single_source(
            std::path::Path::new(crate::account::config::ACCOUNTS_CONFIG_PATH),
            config.env_file_path.as_deref().map(std::path::Path::new),
            now_ms(),
            &config.allowed_round_secs,
        )
        .unwrap_or_else(|e| {
            panic!(
                "refusing to boot: {} (fail-closed: fix the account source and restart)",
                e.message
            )
        });
        Self::with_account_book(config, accounts)
    }

    /// The constructor with the account book SUPPLIED (E28): a multi-account
    /// book for tests without touching the real `user_layer/configs/
    /// accounts.toml` the process cwd decides. Production always enters
    /// through [`Core::new`], which fails closed on a malformed file; this
    /// variant trusts the caller's book.
    pub fn with_account_book(
        config: CoreConfig,
        mut accounts: crate::account::AccountLedgers,
    ) -> Self {
        // Whether the boot below replayed durable cash journals (the server's
        // post-construction seed reads it — a replayed balance must not be
        // re-seeded over).
        let mut boot_replayed_journal = false;
        // #363: the execution policy loads fail-closed at boot. A missing
        // file is the zero-config deployment (code defaults, no account
        // sections); a file that exists but does not parse, misses a field
        // or carries an out-of-range value REFUSES the boot — and leaves one
        // `load-refused` audit line saying so (谁改的/何时/为何, the audit's
        // own first question). A config-path of `None` (backtester, most
        // tests) loads nothing: every default then equals the historical
        // hardcoded behaviour bit for bit.
        //
        // #380: the DEFAULT path in [`CoreConfig::default`] is a SPELLING,
        // not a location — the binary entry (`main.rs`) resolves it against
        // the install root before constructing the config, and logs the
        // resolution (found / neutral-kernel WARN) there. The library
        // honours whatever path it is handed, verbatim, so an embedder (and
        // every in-process test) keeps exactly the semantics it asked for.
        let execution_policy = Self::load_execution_policy_at_boot(&config);
        // #363: the per-asset cap precedence — CLI flag > policy section >
        // compiled 1. The CLI already writes total `max_positions` straight
        // into `config.positions`; this mirrors that for per-asset, reading
        // the value back out of the LOADED policy so the file and the flag
        // cannot disagree silently. A config-path of `None` (or a missing
        // file, which loads the code defaults) leaves the compiled 1 — the
        // historical one-per-asset gate, byte for byte.
        let mut positions = config.positions.clone();
        if let Some(policy) = &execution_policy {
            positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
        }
        if let Some(v) = config.max_positions_per_asset {
            positions.max_positions_per_asset = v;
        }
        let trade_db = config
            .trade_log_path
            .as_ref()
            .map(crate::trade_db::TradeDb::new);
        let order_db = config
            .order_log_path
            .as_ref()
            .map(crate::order_db::OrderDb::new);
        let position_db = config
            .position_log_path
            .as_ref()
            .map(crate::position_db::PositionDb::new);
        // Both side-log paths are derived from the order log's directory when not
        // set explicitly: they belong to the same durable state, and a deployment
        // that persists orders wants its idempotency table and audit trail on
        // disk too. `order_log_path: None` (the backtester, most tests) means no
        // persistence for any of them.
        let side_log_path = |explicit: &Option<String>, name: &str| -> Option<String> {
            if let Some(p) = explicit {
                return Some(p.clone());
            }
            let dir = std::path::Path::new(config.order_log_path.as_deref()?).parent()?;
            Some(dir.join(name).to_string_lossy().to_string())
        };
        let applied_log =
            side_log_path(&config.applied_log_path, "applied-fills.jsonl").map(AppliedFillLog::new);
        // ④ 统一映射账本：有路径就加载持久态（候选→人工确认的映射跨重启
        // 存活），无路径（回测/进程内测试）就纯内存。自动配对只提议，
        // confirm 才写账本 —— ledger 的设计合同，接线必须遵守。
        let unified_ledger = match config.unified_ledger_path.as_deref() {
            Some(p) => blitzkrieg_market_api::unified::MappingLedger::load(std::path::Path::new(p)),
            None => blitzkrieg_market_api::unified::MappingLedger::new(),
        };
        let audit_log = side_log_path(&config.audit_log_path, "reconcile.jsonl");
        let settlement_log = side_log_path(&config.settlement_log_path, "settlements.jsonl");
        // #463: the close-credential journal rides the same derivation — a
        // deployment that persists the order log persists the credentials that
        // let a FAILED unwind one of its closes across a restart.
        let close_credential_log =
            side_log_path(&None, "close-credentials.jsonl").map(std::path::PathBuf::from);
        let settlement = SettlementBook::new(settlement_log.as_deref().map(std::path::Path::new));
        let shadow_cfg = {
            let mut c = ShadowEvolutionConfig {
                enabled: config.shadow_evolution_enabled,
                ..Default::default()
            };
            if let Some(t) = &config.shadow_evolution_tuning {
                if let Some(v) = t.min_sample_count {
                    c.min_sample_count = v;
                }
                if let Some(v) = t.min_win_rate_improvement {
                    c.min_win_rate_improvement = v;
                }
                if let Some(v) = t.min_profit_factor_improvement {
                    c.min_profit_factor_improvement = v;
                }
                if let Some(v) = t.min_observation_secs {
                    c.min_observation_secs = v;
                }
                if let Some(v) = t.cooldown_secs {
                    c.cooldown_secs = v;
                }
                if let Some(v) = t.variant_count {
                    c.variant_count = v;
                }
                if let Some(v) = t.evaluation_window_secs {
                    c.evaluation_window_secs = v;
                }
                if let Some(v) = t.max_gradient {
                    // Belt and braces: the caller clamps too, but this is the one
                    // place the lock could be widened, so it re-checks.
                    c.max_gradient = v.min(crate::config::MAX_GRADIENT_CEILING);
                }
                if let Some(v) = &t.audit_dir {
                    c.audit_dir = v.clone();
                }
                if let Some(v) = t.auto_evolve {
                    c.auto_evolve = v;
                }
                if let Some(v) = t.evolution_cycle_secs {
                    c.evolution_cycle_secs = v;
                }
                if let Some(v) = t.proposal_ttl_secs {
                    c.proposal_ttl_secs = v;
                }
                if let Some(v) = t.deep_dims {
                    c.deep_dims = v.max(1);
                }
            }
            // D-2: variant exits must replay the SAME policy the live position
            // manager runs, or the counterfactual is judged against an exit
            // mechanism the live path never uses.
            c.exit_cfg = config.positions.exit.clone();
            c
        };
        let breaker = LossBreakers::new(config.max_consecutive_losses, config.breaker_cooldown_sec);
        let positions = PositionManager::new(positions);
        // Optional market-data archive (P-1.3). A failure to open only disables
        // recording — trading must never be blocked by an archive path problem.
        let event_archive = config.event_archive_path.as_ref().and_then(|p| {
            let cap_bytes = config.event_archive_max_mb.saturating_mul(1024 * 1024);
            let rotate_bytes = config.event_archive_rotate_mb.saturating_mul(1024 * 1024);
            let min_free_bytes =
                config.event_archive_min_free_mb.saturating_mul(1024 * 1024);
            match crate::data_source::EventArchive::open_full(
                std::path::Path::new(p),
                cap_bytes,
                rotate_bytes,
                min_free_bytes,
            ) {
                Ok(a) => {
                    tracing::info!(
                        path = %p,
                        cap_mb = config.event_archive_max_mb,
                        rotate_mb = config.event_archive_rotate_mb,
                        min_free_mb = config.event_archive_min_free_mb,
                        "recording market-data archive"
                    );
                    Some(a)
                }
                Err(e) => {
                    tracing::warn!(path = %p, error = %e, "cannot open event archive; recording disabled");
                    None
                }
            }
        });
        // Durable cash journals (data/ledger/<account>.journal.jsonl): replay
        // BEFORE seeding. A deployment with journals on disk restores the
        // opening balance from each journal's last row and skips the boot seed
        // — re-seeding over a replay would reset the balance to the seed and
        // vaporize the盈亏 history across every restart. A fresh deployment
        // (no journal files — first boot, or a replay corpus) takes the
        // historical seed_all path, bit for bit. A malformed journal line is
        // skipped with one warn (the jsonl::load contract — the trailing
        // balance comes from the last GOOD row); the file itself is append-
        // only evidence, never rewritten by this path.
        if let Some(dir) = config.ledger_journal_dir.as_deref() {
            let dir = std::path::Path::new(dir);
            let replayed = accounts.any_journal_on_disk(dir);
            accounts.attach_journals(dir);
            // E28 (§9.5): a locally-settling mode has no venue to reconcile
            // against; seed the local cash so the reserve/overspend gate is
            // meaningful for embedders that don't seed it. Uses the predicate,
            // not `== Dry`: the equality form is invisible to the compiler and
            // left ReadOnly funding at zero. EVERY account is seeded (§9.3:
            // the books are separate wallets, not one wallet with aliases);
            // with one account this is exactly the single `seed` 0.2 ran.
            // Skipped when a journal replayed: the balance is a boot-replay
            // fact now, not the seed.
            if config.mode.settles_locally() && !replayed {
                accounts.seed_all(config.dry_seed_balance);
            }
            boot_replayed_journal = replayed;
        } else if config.mode.settles_locally() {
            // E28 (§9.5): a locally-settling mode has no venue to reconcile
            // against; seed the local cash so the reserve/overspend gate is
            // meaningful for embedders that don't seed it. Uses the predicate,
            // not `== Dry`: the equality form is invisible to the compiler and
            // left ReadOnly funding at zero. EVERY account is seeded (§9.3:
            // the books are separate wallets, not one wallet with aliases);
            // with one account this is exactly the single `seed` 0.2 ran.
            accounts.seed_all(config.dry_seed_balance);
        }
        let mut intent_audit = crate::arbitration::AuditSink::new(accounts.active_id().as_str());
        intent_audit.enabled = config.intent_audit_enabled;
        // 跨所套利弹窗 v2: the leg configuration reads from the SAME source
        // the book came from (issue 751: ACCOUNT_* env — process env, then
        // the `.env` file — with the toml file as the migration-window
        // fallback). The parse layer validates coherence (from_str /
        // load_account_ledgers_from_env refuse an incoherent section), so a
        // read here after a successful book load cannot fail semantically —
        // the fallback below is reachable only when the source vanished
        // between the two reads, and then "unconfigured" is the honest
        // answer.
        let arb_leg_config = {
            let from_env = crate::account::config::read_cross_venue_arb_from_env(
                &crate::account::config::env_lookup_from_sources(
                    config.env_file_path.as_deref().map(std::path::Path::new),
                ),
            )
            .map_err(|e| {
                tracing::warn!(
                    error = %e.message,
                    "cross_venue_arb env section invalid at boot — running unconfigured"
                );
            })
            .ok()
            .flatten();
            match from_env {
                Some(cfg) => Some(cfg),
                None => config
                    .accounts_config_path
                    .as_deref()
                    .map(std::path::Path::new)
                    .and_then(|p| {
                        crate::account::config::read_cross_venue_arb(p)
                            .map_err(|e| {
                                tracing::warn!(
                                    error = %e.message,
                                    "cross_venue_arb section unreadable at boot — running unconfigured"
                                );
                            })
                            .ok()
                            .flatten()
                    }),
            }
        }
            .inspect(|cfg| {
                tracing::info!(
                    leg_a = %format!("{}/{}", cfg.leg_a_venue, cfg.leg_a_account),
                    leg_b = %format!("{}/{}", cfg.leg_b_venue, cfg.leg_b_account),
                    pairs = ?cfg.pairs,
                    "cross-venue arb legs configured (accounts.toml [cross_venue_arb])"
                );
            });
        // E-VA: the virtual pots are built from the config's caps BEFORE the
        // config moves into the struct (the caps are copied, so a later
        // runtime config mutation cannot move a quota mid-session).
        let virtual_accounts =
            crate::risk::virtual_account::VirtualAccounts::new(&config.virtual_limits);
        // The shadow manager reads the switch write-back path off the config
        // before the config moves into the struct (same ordering concern as
        // above). Issue 751: the flips land in `.env`, not the retired
        // `shadow_evolution.toml`.
        let shadow_env_path = config.env_file_path.clone();
        Self {
            risk: RiskGate::new(config.risk.clone()),
            config,
            ome: Ome::new(),
            inflight_takers: Vec::new(),
            accounts,
            breaker,
            intent_audit,
            positions,
            books: HashMap::new(),
            exit_reason_ledger: ExitReasonLedger {
                exit_reasons: HashMap::new(),
                exit_reason_order: VecDeque::new(),
                exit_reasons_evicted: 0,
                exit_reasons_swept: 0,
            },
            strategy_exits: Vec::new(),
            engine: None,
            strategy_declarations: HashMap::new(),
            plugin_modes: Vec::new(),
            feeds: std::collections::BTreeMap::new(),
            extensions: {
                use crate::extension::Extension;
                let mut reg = crate::extension::ExtensionRegistry::new();
                // Built-in example extension (installed, not enabled by default).
                // Its config file is read and checked against the registered
                // extension (KI-11): the file is no longer inert, and a config
                // that has drifted from the code is reported at startup.
                let ext = crate::extension::builtins::BinanceSpotExtension::new();
                let path =
                    crate::extension::config_path_for(crate::extension::EXTENSIONS_DIR, ext.name());
                let cfg = crate::config::ExtensionConfig::load(&path);
                for w in &cfg.warnings {
                    tracing::warn!(extension = ext.name(), "extension config: {w}");
                }
                for k in &cfg.unknown_keys {
                    tracing::warn!(
                        extension = ext.name(),
                        key = k,
                        "extension config key not understood (ignored)"
                    );
                }
                for d in &cfg.declared_only {
                    tracing::info!(
                        extension = ext.name(),
                        key = %d.key,
                        reason = d.reason,
                        "extension config declares a setting the kernel does not act on"
                    );
                }
                let kind = format!("{:?}", ext.extension_type()).to_lowercase();
                for problem in cfg.check_against(ext.name(), ext.version(), &kind) {
                    tracing::warn!(extension = ext.name(), "extension config: {problem}");
                }
                reg.install(Box::new(ext), cfg.path);
                reg
            },
            shadow_evolution: ShadowEvolution::new_with_env_path(shadow_cfg, &[], shadow_env_path),
            trade_db,
            order_db,
            position_db,
            stats: CoreStats::default(),
            // 探针从零开始：没有 tick/spot/NTP 历史（与 stats 同一生命期，
            // 会话作用域）。
            probe_health: ProbeHealth {
                probe_ticks: crate::probe::TickHealth::default(),
                probe_spot: crate::probe::SpotWatch::default(),
                probe_ntp: crate::probe::NtpWatch::default(),
                probe_last_window_ticks: 0,
                probe_last_zero: Vec::new(),
                probe_last_max_silence_ms: 0,
                probe_last_spot_stale_ms: None,
                stats_probe_stream_breaks: 0,
                stats_probe_spot_stales: 0,
            },
            archive_verdicts: HashSet::new(),
            // The session clock for the accounting identity (issue #200). Read
            // once, at construction: the ledger below is seeded at the same
            // instant, so "cash since the seed" and "trades since startedAtMs"
            // describe the same window.
            started_at_ms: now_ms(),
            event_archive,
            strategy_accounting: HashMap::new(),
            virtual_accounts,
            place_cooldowns: HashMap::new(),
            escalation_skips: HashSet::new(),
            pending_venue_cancels: Vec::new(),
            consecutive_venue_rejects: 0,
            consecutive_venue_rejects_by_account: HashMap::new(),
            account_freezes: HashMap::new(),
            auto_frozen: HashMap::new(),
            consecutive_sweep_failures: 0,
            consecutive_sweep_failures_by_venue: HashMap::new(),
            last_error: None,
            recon_watermark_ms: 0,
            boot_replayed_journal,
            last_self_check: None,
            applied_log,
            unified_ledger,
            unified_candidates: Vec::new(),
            audit: AuditRuntime {
                audit_log_path: audit_log,
                ..Default::default()
            },
            settlement,
            venue_free_by_venue: std::collections::HashMap::new(),
            archive_recording_last: true,
            ghost_cash_alerted: std::collections::HashMap::new(),
            venue_truth_last: std::collections::HashMap::new(),
            venue_fact_cursor: std::collections::HashMap::new(),
            close_credentials: std::collections::VecDeque::new(),
            close_credential_log,
            next_id: 1,
            tx: None,
            kline_preview_last_ms: std::collections::HashMap::new(),
            // #363: the boot-loaded policy (None = no config file found).
            execution_policy,
            // #757: nothing written through IPC yet — every key resolves from
            // the file / deployment env exactly as at boot.
            execution_policy_env_writes: std::collections::BTreeMap::new(),
            policy_cooldowns: HashMap::new(),
            // Replay fast path: nothing dirty, no deadlines known yet — the
            // first tick always runs the full sweeps (MAX stamps force them).
            fast_path: ReplayFastPath {
                fast_dirty_tokens: HashSet::new(),
                fast_eval_wake: true,
                fast_next_timing_ms: 0,
                fast_orders_dirty: true,
                fast_positions_dirty: true,
                fast_next_escalation_ms: 0,
                fast_next_expiry_ms: 0,
                fast_last_eval_ms: 0,
                // Issue #390 (nofast): 0 = due on the first tick.
                fast_next_exit_deadline_ms: 0,
            },
            pair_chase_attempts: HashMap::new(),
            #[cfg(test)]
            test_duration_subscriptions: None,
            #[cfg(test)]
            test_escalation_snapshot_fill: None,
            // 跨所套利弹窗 v2: the boot-loaded leg configuration (memory
            // mirror of accounts.toml's `[cross_venue_arb]` — the file is the
            // durable truth, this cell feeds the engine hot). None =
            // unconfigured: the legacy unified-confirm chain stays the only
            // pairing source and the account-stamp is inert.
            arb_leg_config,
            // RFC 568 阶段一：配对簿随进程启动为空账（不持久化 —— 事实事件
            // 重放经同一事件流重建，#559 3c 的口径见 pair 模块注释）。
            pair_book: crate::pair::PairBook::default(),
        }
    }

    /// #363: the boot-time policy load. Fail-closed on ANY malformed input;
    /// the refusal is both panicked (the boot must not continue) and audited
    /// (`load-refused`), so an operator who walks up to a dead kernel finds
    /// the reason in the journal, not only in a scrolled-away log line.
    fn load_execution_policy_at_boot(
        config: &CoreConfig,
    ) -> Option<crate::execution_policy::Policy> {
        let path = config
            .execution_policy_path
            .as_deref()
            .map(std::path::Path::new)?;
        // Single-source read (issue 751): the launcher folds .env into the
        // process environment, but the file face is read too so a same-checkout
        // tool spawn (no launcher) still sees the operator's POLICY_* keys.
        let env_file = config.env_file_path.as_deref().map(std::path::Path::new);
        match crate::execution_policy::Policy::load_single_source(path, env_file) {
            Ok(policy) => Some(policy),
            Err(reason) => {
                crate::execution_policy::append_audit_to(
                    &Self::execution_policy_audit_file(config),
                    &crate::execution_policy::PolicyAuditRecord {
                        ts_ms: now_ms(),
                        actor: crate::execution_policy::ACTOR_BOOT.to_string(),
                        action: "load-refused".to_string(),
                        account_id: "*".to_string(),
                        before: None,
                        after: None,
                        error: Some(reason.clone()),
                    },
                );
                panic!(
                    "refusing to boot: {reason} (fail-closed: fix execution_policy.toml and \
                     restart)"
                );
            }
        }
    }

    // ── #363: execution policy surface (IPC + reload) ──────────────────────

    /// One section as the wire reports it: the EFFECTIVE view (own overrides
    /// folded over the globals), decimals as strings so the shortest-decimal
    /// rule survives the JSON hop.
    pub fn execution_policy_section_view(
        &self,
        account_id: &str,
    ) -> Option<crate::schema::ExecutionPolicySectionView> {
        let policy = self.execution_policy.as_ref()?;
        let s = policy.section_for(account_id);
        Some(crate::schema::ExecutionPolicySectionView {
            budget_ratio: Some(s.budget_ratio.to_string()),
            min_budget_usd: Some(s.min_budget_usd.to_string()),
            max_budget_usd: Some(s.max_budget_usd.to_string()),
            min_equity_usd: Some(s.min_equity_usd.to_string()),
            max_positions_per_asset: s.max_positions_per_asset,
            rules: Some(
                s.rules
                    .iter()
                    .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                    .collect(),
            ),
        })
    }

    /// The account ids the loaded policy names with a section of their own.
    pub fn execution_policy_account_ids(&self) -> Vec<String> {
        match &self.execution_policy {
            Some(p) => {
                let mut ids: Vec<String> = p.accounts.keys().cloned().collect();
                ids.sort();
                ids
            }
            None => Vec::new(),
        }
    }

    /// The journal file this config's execution-policy audit lines land in
    /// (#386): the config's private override, or the shipped
    /// `data/audit/execution_policy.jsonl` spelling when unset.
    fn execution_policy_audit_file(config: &CoreConfig) -> std::path::PathBuf {
        config
            .execution_policy_audit_path
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(crate::execution_policy::EXECUTION_POLICY_AUDIT_PATH)
            })
    }

    /// Whether the policy is loaded at all (the zero-config deployment has
    /// none — the read arms report that instead of inventing a section).
    pub fn execution_policy_loaded(&self) -> bool {
        self.execution_policy.is_some()
    }

    /// Append one audit line to THIS core's journal (#386). The wire set/
    /// reset arms and the reload refusal all land through here, so a test
    /// core with a private journal never touches the checkout's data dir.
    pub fn execution_policy_append_audit(
        &self,
        record: &crate::execution_policy::PolicyAuditRecord,
    ) -> bool {
        crate::execution_policy::append_audit_to(
            &Self::execution_policy_audit_file(&self.config),
            record,
        )
    }

    /// The audit journal for the history arm — read from THIS core's own
    /// journal file (#386), not a process-wide fixed path.
    pub fn execution_policy_audit(&self) -> Vec<crate::execution_policy::PolicyAuditRecord> {
        crate::execution_policy::load_audit_from(&Self::execution_policy_audit_file(&self.config))
    }

    /// #364: replay the CURRENT section over the recent closed trades of one
    /// account — the read-only fact behind the UI's "these rules would skip
    /// N of the last M entries". Every historical close supplies the facts a
    /// live entry would carry (balance is the LIVE ledger, not a per-trade
    /// snapshot: a ledger is a book, not a tape — disclosed in the row, not
    /// pretended into history), and `evaluate` — the ONE verdict function —
    /// judges it. The preview can never disagree with the kernel because it
    /// does not re-implement the kernel.
    pub fn execution_policy_preview(
        &self,
        account_id: &str,
        limit: usize,
    ) -> crate::schema::ExecutionPolicyPreviewResult {
        use crate::execution_policy::PolicyOutput;
        let now = now_ms();
        // Closed trades of THIS account, oldest first, capped at `limit` —
        // the same rows `trades.history` reports, judged instead of shown.
        let mut closed: Vec<&crate::position::ClosedPosition> = self
            .positions
            .closed_positions()
            .iter()
            .filter(|c| c.account_id.as_str() == account_id)
            .collect();
        if closed.len() > limit {
            closed.drain(0..closed.len() - limit);
        }
        // The live book facts (the parts of PolicyInput that are not
        // per-trade): balance/equity from the account's own ledger, the
        // trailing-hour PnL, the open count, the strategy streaks.
        let balance = self
            .accounts
            .ledger_of(&blitzkrieg_market_api::AccountId::from(account_id))
            .map(|l| l.balance())
            .unwrap_or_default();
        let open_for = self
            .positions
            .open_positions()
            .iter()
            // The preview replays the entry gates: the count must match the
            // entry path's LIVE-only rule (expired rounds are settlement-dead).
            .filter(|p| p.account_id.as_str() == account_id && p.expires_at_ms > now)
            .count() as u32;
        let mut considered = 0usize;
        let mut skipped = 0usize;
        let mut budgets: Vec<Decimal> = Vec::new();
        let mut rows = Vec::with_capacity(closed.len());
        for c in &closed {
            considered += 1;
            let facts = crate::execution_policy::PolicyInput {
                available_balance: balance,
                total_equity: balance,
                open_positions: open_for,
                current_price: c.entry_price,
                // The close is long over; the round it belonged to has
                // resolved. Zero is the honest "no clock running" fact.
                time_left_sec: 0,
                symbol: c.asset.clone(),
                recent_pnl_1h: self.positions.recent_pnl_1h_for(account_id, now),
                consecutive_losses: self.breaker.consecutive_losses(&c.strategy),
            };
            let (verdict, detail) = match self
                .execution_policy
                .as_ref()
                .map(|p| crate::execution_policy::evaluate(p, account_id, &facts))
            {
                None => ("refused".to_string(), Some("no policy loaded".to_string())),
                Some(Err(refusal)) => ("refused".to_string(), Some(refusal.reason)),
                Some(Ok(PolicyOutput::Place { budget_usd, .. })) => {
                    budgets.push(budget_usd);
                    ("place".to_string(), Some(budget_usd.to_string()))
                }
                Some(Ok(PolicyOutput::Skip { reason })) => {
                    skipped += 1;
                    ("skip".to_string(), Some(reason))
                }
                Some(Ok(PolicyOutput::Cooldown { seconds })) => {
                    skipped += 1;
                    ("cooldown".to_string(), Some(format!("{seconds}s")))
                }
            };
            let symbol = facts.symbol.clone();
            rows.push(crate::schema::ExecutionPolicyPreviewRow {
                ts_ms: c.exited_at_ms,
                symbol,
                balance,
                price: c.entry_price,
                verdict,
                detail,
            });
        }
        // `in` conditions on symbols judge the asset the trade named; nothing
        // special-cased here — `evaluate` already handles every field.
        let avg_budget_usd = if budgets.is_empty() {
            None
        } else {
            let sum = budgets.iter().copied().fold(Decimal::ZERO, |a, b| a + b);
            let n = Decimal::from(budgets.len() as u64);
            // Shortest-decimal discipline applies to the WIRE (the `to_string`
            // below is what crosses IPC), not to the Decimal itself — no
            // `normalize()` here, it would shave 10.00 down to "10" and break
            // the money-string convention every other field keeps.
            Some((sum / n).to_string())
        };
        crate::schema::ExecutionPolicyPreviewResult {
            considered,
            skipped,
            avg_budget_usd,
            rows,
        }
    }

    /// #757: remember the keys the IPC section write just made authoritative
    /// (`Some` = the value written, `None` = the key was swept). The write
    /// face calls this BEFORE [`Self::reload_execution_policy`] so the reload
    /// resolves those keys from memory rather than the stale startup export
    /// of the process environment.
    pub fn execution_policy_record_env_write(
        &mut self,
        written: std::collections::BTreeMap<String, Option<String>>,
    ) {
        self.execution_policy_env_writes.extend(written);
    }

    /// Replace the in-memory policy wholesale after a file write (set/reset
    /// arms). Re-derives the per-asset cap the way boot did — CLI flag wins
    /// over the file — and lands one `load-refused` audit line + keeps the
    /// OLD policy when the new file does not parse: fail-closed means a
    /// broken write never trades unguarded, it keeps the last good guard.
    pub fn reload_execution_policy(&mut self) -> Result<(), String> {
        let path = self
            .config
            .execution_policy_path
            .clone()
            .ok_or_else(|| "execution policy disabled (no config path)".to_string())?;
        // Single-source read (issue 751): the write face lands POLICY_* keys
        // in the .env the launcher owns — a same-process reload must see them
        // before the next restart folds the file into the process env. The
        // in-memory write snapshot (#757) rides between the file and the
        // process env so a stale boot export can neither shadow a write nor
        // resurface after a reset.
        let env_file = self
            .config
            .env_file_path
            .as_deref()
            .map(std::path::Path::new)
            .ok_or_else(|| "env_file_path not set".to_string())?;
        match crate::execution_policy::Policy::load_for_reload(
            std::path::Path::new(&path),
            env_file,
            &self.execution_policy_env_writes,
        ) {
            Ok(policy) => {
                let mut positions = self.config.positions.clone();
                positions.max_positions_per_asset = policy.defaults.max_positions_per_asset;
                if let Some(v) = self.config.max_positions_per_asset {
                    positions.max_positions_per_asset = v;
                }
                self.positions.set_config(positions);
                self.execution_policy = Some(policy);
                Ok(())
            }
            Err(reason) => {
                self.execution_policy_append_audit(&crate::execution_policy::PolicyAuditRecord {
                    ts_ms: now_ms(),
                    actor: crate::execution_policy::ACTOR_IPC.to_string(),
                    action: "load-refused".to_string(),
                    account_id: "*".to_string(),
                    before: None,
                    after: None,
                    error: Some(reason.clone()),
                });
                Err(reason)
            }
        }
    }

    /// Reload account configuration from .env (ACCOUNT_* keys).
    /// Fails closed if the .env is malformed, keeping the current configuration.
    pub fn reload_accounts(&mut self) -> Result<(), String> {
        let env_file = self
            .config
            .env_file_path
            .as_deref()
            .map(std::path::Path::new)
            .ok_or_else(|| "env_file_path not set".to_string())?;
        let path = std::path::Path::new(crate::account::config::ACCOUNTS_CONFIG_PATH);
        let now_ms = blitzkrieg_market_api::net::now_ms();
        let allowed = &self.config.allowed_round_secs;
        match crate::account::config::load_account_ledgers_for_reload(
            path, env_file, now_ms, allowed,
        ) {
            Ok(accounts) => self.accounts.reload_config(accounts),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Reload shadow evolution switches from .env (SHADOW_* keys).
    /// Fails closed if the .env is malformed, keeping the current configuration.
    pub fn reload_shadow(&mut self) -> Result<(), String> {
        let env_path = self
            .config
            .env_file_path
            .as_deref()
            .map(std::path::Path::new);

        let env_path = env_path.ok_or_else(|| "env_file_path not set".to_string())?;

        // Read the .env file and find SHADOW_ENABLED and SHADOW_AUTO_EVOLVE
        let content =
            std::fs::read_to_string(env_path).map_err(|e| format!("failed to read .env: {}", e))?;

        let mut enabled = false;
        let mut auto_evolve = false;

        for line in content.lines() {
            if let Some((key, value)) = crate::env_file::parse_line(line) {
                match key.as_str() {
                    "SHADOW_ENABLED" => {
                        enabled = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                return Err(format!(
                                    "SHADOW_ENABLED must be true/false, got: {}",
                                    value
                                ));
                            }
                        };
                    }
                    "SHADOW_AUTO_EVOLVE" => {
                        auto_evolve = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                return Err(format!(
                                    "SHADOW_AUTO_EVOLVE must be true/false, got: {}",
                                    value
                                ));
                            }
                        };
                    }
                    _ => {}
                }
            }
        }

        if enabled {
            self.shadow_evolution
                .enable(blitzkrieg_market_api::net::now_ms() as i64);
        } else {
            self.shadow_evolution.disable();
        }
        self.shadow_evolution.set_auto_evolve(auto_evolve);
        Ok(())
    }

    pub fn set_event_sink(&mut self, tx: mpsc::UnboundedSender<Event>) {
        self.tx = Some(tx);
    }
}
