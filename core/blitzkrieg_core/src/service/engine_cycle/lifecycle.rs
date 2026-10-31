//! Engine lifecycle wiring and feed-health probes (#780): pure move
//! from `service/engine_cycle`.

use super::*;

impl Core {
    /// Attach the per-strategy parameter registry to the driving engine so an
    /// evolution takes effect on the next tick with no restart. Also re-reads the
    /// strategies' knob declarations (their twins are built from them).
    ///
    /// While evolution is DISABLED the overlay is detached (`None`): strategies
    /// then run on the config the host pushed, which is byte-for-byte the
    /// pre-evolution behaviour (acceptance: "关闭进化时行为与改动前一致").
    pub(crate) fn rewire_hot_params(&mut self, now_ms: i64) {
        if let Some(e) = self.engine.as_mut() {
            if self.shadow_evolution.is_enabled() {
                // The whole host set, enabled or not: a declaration is read off the
                // live instance (E2-c), and `register_strategies` removes the cell of
                // anything ABSENT — so filtering by `enabled` here would delete a
                // switched-off strategy's evolved parameters and rollback anchor,
                // making a toggle lossy. A disabled strategy simply never emits the
                // candidates that would consume them.
                //
                // `now_ms` is the clock the CALLER acts at, forwarded rather than
                // re-read: this re-registration re-anchors every variant, and the
                // birth stamp it writes is what the observation floor is measured
                // against — one operation, one clock (#250).
                self.shadow_evolution
                    .register_strategies(&e.strategy_refs(), now_ms);
                e.set_hot_params(Some(self.shadow_evolution.registry()));
            } else {
                e.set_hot_params(None);
            }
        }
        // The strategies' trailing declarations ride the same registry-change
        // beat: a whole-map replace (stale entries of unregistered strategies
        // must not linger) into the position book, whose ladder then overlays
        // each verdict on the kernel-wide value. Registration, unregistration
        // and account-scope wiring all pass through here.
        let overrides: std::collections::HashMap<String, bool> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_refs()
                    .into_iter()
                    .filter_map(|s| s.wants_trailing_exit().map(|v| (s.name().to_string(), v)))
                    .collect()
            })
            .unwrap_or_default();
        self.positions.set_trailing_overrides(overrides);
        // Hold-to-settlement declarations ride the SAME beat: a holder's
        // positions get the WIDE containment stop (`ExitConfig::holder_stop_
        // pct`) instead of the 12% ladder — the @almach compromise for the
        // live ladder that keeps walking until venue settlement is proven.
        // Whole-map replace, same staleness rule.
        let holder_stops: std::collections::HashMap<String, Decimal> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_refs()
                    .into_iter()
                    .filter(|s| s.holds_to_settlement())
                    .map(|s| {
                        (
                            s.name().to_string(),
                            self.config.positions.exit.holder_stop_pct,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.positions.set_holder_stop_overrides(holder_stops);
    }

    pub(crate) fn emit_shadow_outcome(&self, outcome: EvolutionOutcome) {
        match outcome {
            EvolutionOutcome::Signal(sig) => self.emit(Event::EvolutionSignal { signal: sig }),
            EvolutionOutcome::Applied(sig) => self.emit(Event::EvolutionApplied { signal: sig }),
            EvolutionOutcome::Proposed(proposal) => {
                self.emit(Event::EvolutionProposed { proposal })
            }
            EvolutionOutcome::Rejected { signal, reason } => {
                self.emit(Event::EvolutionRejected { signal, reason })
            }
            EvolutionOutcome::RolledBack { strategy, from, to } => {
                // Rollback restores the pre-change parameters for ONE strategy; the
                // audit file already records it. Emitting an applied-shaped signal
                // keeps the UI's "parameters changed" path single.
                self.emit(Event::EvolutionApplied {
                    signal: crate::shadow_evolution::EvolveSignal::new(
                        format!("rollback-{strategy}"),
                        blitzkrieg_market_api::net::now_ms(),
                        strategy,
                        from,
                        to,
                        crate::shadow_evolution::EvolutionReason::CombinedImprovement,
                        rust_decimal::Decimal::ONE,
                        0,
                        rust_decimal::Decimal::ZERO,
                        "rollback".into(),
                    ),
                })
            }
        }
    }

    // ── Self-driving engine (P3) ────────────────────────────────────────────
    pub fn enable_engine(&mut self, engine: crate::engine::Engine) {
        self.engine = Some(engine);
        // E2-c: each strategy declares its own evolvable knobs, so the manager can
        // only build its units once the strategies exist. This also attaches the
        // parameter registry, so installing an engine never leaves the overlay
        // unwired.
        self.rewire_hot_params(now_ms());
    }

    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// Read view of the driving engine for readers OUTSIDE `service` (the IPC
    /// view assembly, market-host diagnostics). The field itself is private
    /// (#780 boundary hygiene): every mutation goes through
    /// [`Self::enable_engine`] (install — which wires the hot-params overlay
    /// and re-reads knob declarations) or the two replay seams below, so a
    /// naked write can never bypass the install-time invariants.
    pub fn engine(&self) -> Option<&crate::engine::Engine> {
        self.engine.as_ref()
    }

    /// Replay seam (the backtest host path): register one additional user
    /// strategy into the running engine WITHOUT going through the loader —
    /// the same `EngineStrategy` contract a loaded library implements. A
    /// silent no-op without an engine (a hostless replay has nothing to host
    /// into, and no ghost registration may materialize), and like every
    /// registration the strategy starts DISABLED — enabling it stays an
    /// explicit, separate decision.
    pub fn host_replay_strategy(
        &mut self,
        strategy: Box<dyn crate::strategies::EngineStrategy>,
        source: &str,
    ) -> Result<(), String> {
        let Some(engine) = self.engine.as_mut() else {
            return Ok(());
        };
        engine
            .register_user_strategy(strategy, source.to_string())
            .map(|_| ())
    }

    /// Replay seam (counterfactual knobs): attach or detach the hot-params
    /// registry — the SAME cell a Shadow Evolution proposal writes, so a
    /// replay's knobs cannot diverge from the live overlay semantics. A no-op
    /// without an engine. `None` detaches: strategies fall back to the config
    /// pushed via `on_config` (the observable form of "evolution off").
    pub fn set_engine_hot_params(
        &mut self,
        registry: Option<std::sync::Arc<crate::shadow_evolution::ParamRegistry>>,
    ) {
        if let Some(engine) = self.engine.as_mut() {
            engine.set_hot_params(registry);
        }
    }

    /// Whether the driving engine currently holds a Shadow Evolution parameter
    /// overlay (E2-c / #28). `false` while evolution is off, because the overlay
    /// is DETACHED rather than merely ignored — the observable form of the
    /// acceptance criterion "evolution off ⇒ behaviour unchanged". A strategy can
    /// then only read the config the host pushed through `on_config`.
    pub fn has_hot_params(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.has_hot_params())
    }

    /// Install the running data feed's subscription control (P4), keyed by the
    /// market-plugin name. Called by the market plugin when its `DataFeed`
    /// starts, so the core stays unaware of any concrete feed implementation.
    /// A second install for the same name replaces the first (feed restart).
    pub fn set_subscription(
        &mut self,
        plugin: &str,
        control: std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    ) {
        self.feeds.insert(plugin.to_string(), control);
    }

    /// NTP 循环（`probe::spawn_loop`，生产 `server::run()` 专用）把一次测量
    /// 结果灌回内核。`None` = 查询失败（静默降级，只计失败数）。偏移达到
    /// 警告线（50ms）打一条 WARNING——这是"系统时间偏差超过 100ms 且无日志
    /// 警告 → 测试必须红"验收的日志面；红线（100ms）在
    /// `engine.stats.probe.ntp.skewRed` 打标。
    pub fn note_ntp_offset(&mut self, offset_ms: Option<i64>, now_ms: i64) {
        let warn = self.probe_ntp.apply(offset_ms, now_ms);
        if warn {
            let ms = self.probe_ntp.last_offset_ms.unwrap_or_default();
            let red = self.probe_ntp.skew_red();
            tracing::warn!(
                offset_ms = ms,
                red_line = red,
                "clock offset: local clock is {}{} ms from the NTP server{}",
                if ms >= 0 { "+" } else { "" },
                ms,
                if red {
                    " (RED: at or past the 100 ms line)"
                } else {
                    ""
                },
            );
        }
    }

    /// 探针汇总到点了吗：首个窗口从第一批数据（tick/spot/round）起算满
    /// 10s，之后每 10s 一次。间隔判定基于调用方的时钟（间隔不等式，不是
    /// 固定墙钟窗口——memory: wallclock-test-flake 处方）。
    pub(crate) fn probe_summary_due(&self, now_ms: i64) -> bool {
        match self.probe_ticks.last_summary_ms {
            None => self
                .probe_ticks
                .first_data_ms
                .is_some_and(|first| now_ms - first >= crate::probe::SUMMARY_INTERVAL_MS),
            Some(last) => now_ms - last >= crate::probe::SUMMARY_INTERVAL_MS,
        }
    }

    /// 执行一次探针 10s 汇总：窗口 tick 数、零 tick token、最长静默、
    /// 新断流 token（warn + 计数）与 spot 停更（warn + 计数）。
    /// 只观测——这里发出的每一个字都是日志，不改变任何交易语义。
    pub(crate) fn probe_run_summary(&mut self, now_ms: i64) {
        // 本轮已声明的 token（零 tick 点名对象）；无引擎 = 无名单。
        let round_tokens: Vec<String> = self
            .engine
            .as_ref()
            .map(|e| e.round_token_ids())
            .unwrap_or_default();
        let (ticks, zero, max_silence, newly_broken) =
            self.probe_ticks
                .summarize(now_ms, &round_tokens, crate::probe::TICK_STALE_WARN_MS);
        // 把 venue 标签留在断流行里——排查时"哪个所的流断了"是第一问。
        for token in &newly_broken {
            let (venue, silence_ms) = match self.probe_ticks.per_token.get(token) {
                Some((v, at)) => (v.clone(), now_ms - *at),
                None => (String::new(), 0),
            };
            self.stats_probe_stream_break();
            tracing::warn!(
                token = %token,
                venue = %venue,
                silence_ms = silence_ms,
                "stream break: token had ticks before but none for the budget now"
            );
        }
        if !zero.is_empty() {
            tracing::warn!(
                tokens = %zero.join(","),
                count = zero.len(),
                "zero tick: round tokens with no data this window"
            );
        }
        if ticks > 0 || !zero.is_empty() || !newly_broken.is_empty() {
            tracing::debug!(
                window_ticks = ticks,
                zero_tick_tokens = zero.len(),
                max_silence_ms = max_silence,
                "tick health summary"
            );
        }
        self.probe_last_window_ticks = ticks;
        self.probe_last_zero = zero;
        self.probe_last_max_silence_ms = max_silence;

        // spot 停更（信号失效监控）。
        let (spot_warn, stale) = self
            .probe_spot
            .check(now_ms, crate::probe::SPOT_STALE_WARN_MS);
        if spot_warn {
            self.stats_probe_spot_stale();
            tracing::warn!(
                stale_ms = stale.unwrap_or_default(),
                "spot feed stale: no spot update for the budget — signals pricing off it \
                 are unverified (signal invalidation watch)"
            );
        }
        self.probe_last_spot_stale_ms = stale;
    }

    // 探针的三个曝光计数（stats 面从 probe_ticks/probe_spot 直接读，
    // 这两个只是把 warn 事件也钉在 CoreStats 里以防未来挪动）。
    pub(super) fn stats_probe_stream_break(&mut self) {
        self.stats_probe_stream_breaks += 1;
    }

    pub(super) fn stats_probe_spot_stale(&mut self) {
        self.stats_probe_spot_stales += 1;
    }
}

#[cfg(test)]
mod replay_seam_tests {
    //! #780 boundary hygiene: the engine field is private, so the replay
    //! seams are the ONLY way backtest/IPC touch it. Negative cases: hosting
    //! or param-swapping without an engine must not materialize ghost state,
    //! and hosting must never auto-enable the strategy it registers.

    use super::*;

    // The test module builds bare cores directly; `CoreConfig` is not part of
    // the parent's wildcard re-exports.
    use crate::service::CoreConfig;

    #[test]
    fn hosting_without_an_engine_is_a_silent_noop_with_no_ghost_state() {
        let mut c = Core::new(CoreConfig::default());
        assert!(!c.has_engine());
        let hosted = c.host_replay_strategy(
            Box::new(crate::strategies::test_support::TestSpreadArb::new(
                Default::default(),
                Default::default(),
            )),
            "replay-hosted",
        );
        assert!(hosted.is_ok(), "hosting without an engine must not error");
        assert!(c.engine().is_none());
        assert!(
            c.enabled_strategy_names().is_empty(),
            "no engine — no strategy rows may appear"
        );
    }

    #[test]
    fn a_replay_hosted_strategy_starts_disabled() {
        let mut c = Core::new(CoreConfig::default());
        c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig::default()));
        c.host_replay_strategy(
            Box::new(crate::strategies::test_support::TestSpreadArb::new(
                Default::default(),
                Default::default(),
            )),
            "replay-hosted",
        )
        .expect("hosting into a present engine registers");
        assert!(c.engine().is_some());
        assert!(
            !c.enabled_strategy_names().contains(&"spread_arb".to_string()),
            "registration starts DISABLED; the seam must not enable it"
        );
    }

    #[test]
    fn hot_params_seam_without_an_engine_never_materializes_an_overlay() {
        let mut c = Core::new(CoreConfig::default());
        c.set_engine_hot_params(Some(std::sync::Arc::new(
            crate::shadow_evolution::ParamRegistry::new(),
        )));
        assert!(!c.has_hot_params(), "no engine — no ghost overlay");
        c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig::default()));
        c.set_engine_hot_params(Some(std::sync::Arc::new(
            crate::shadow_evolution::ParamRegistry::new(),
        )));
        assert!(c.has_hot_params(), "the seam drives the same cell");
        c.set_engine_hot_params(None);
        assert!(!c.has_hot_params(), "None detaches the overlay");
    }

    #[test]
    fn tick_schedule_setter_normalizes_a_non_positive_grid_step() {
        // #390/#780: the ladder's deadline schedule backfills its counter
        // ratchets over the grid the caller will actually drive — a
        // non-positive step would corrupt that schedule. The replay driver
        // used to apply the `.max(1)` floor at its call site through the
        // naked `config_mut` write; the narrow mutator owns it now.
        let mut c = Core::new(CoreConfig::default());
        c.set_tick_schedule_ms(0);
        assert_eq!(c.config().tick_schedule_ms, 1);
        c.set_tick_schedule_ms(-50);
        assert_eq!(c.config().tick_schedule_ms, 1);
        c.set_tick_schedule_ms(250);
        assert_eq!(c.config().tick_schedule_ms, 250);
    }
}
