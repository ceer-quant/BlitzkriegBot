//! The self-driving strategy engine's integration with the core, moved
//! verbatim from `service.rs` (pure code motion, no logic edits): the
//! market-data → engine feed, the Lua evaluation cycle with its replay
//! fast-path gates, the sizing/risk pre-filters (systemic facts, portfolio
//! and per-strategy limits, virtual-account quota, the cross-venue arb
//! pre-check), the diagnostics/stats readouts and the feed-health probes.

use crate::engine::BatchBook;
use crate::ipc::schema::Event;
use crate::model::*;
use crate::shadow_evolution::EvolutionOutcome;
use crate::sim::{Book, rests_on_book};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

use super::Core;
use super::FAST_EVAL_FALLBACK_MS;
use super::config::StrategyLimit;
use super::helpers::{
    ARB_STRATEGY, SWEEP_FAILURE_FREEZE, arb_name_branch_warn_once, classify_rejection,
    strategy_acc_key,
};
use super::helpers::{
    CycleLeg, StrategyAccounting, dec_json, per_account_shares, virtual_batch_commit_impl,
};
use super::views::{AssetBooksView, BookLevelView, BookSideView, MarketPriceView, RoundView};
use crate::market::{venue_credentials_missing, venue_credentials_ready};

impl Core {
    /// Attach the per-strategy parameter registry to the driving engine so an
    /// evolution takes effect on the next tick with no restart. Also re-reads the
    /// strategies' knob declarations (their twins are built from them).
    ///
    /// While evolution is DISABLED the overlay is detached (`None`): strategies
    /// then run on the config the host pushed, which is byte-for-byte the
    /// pre-evolution behaviour (acceptance: "关闭进化时行为与改动前一致").
    pub(super) fn rewire_hot_params(&mut self, now_ms: i64) {
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

    pub(super) fn emit_shadow_outcome(&self, outcome: EvolutionOutcome) {
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
    pub(super) fn probe_summary_due(&self, now_ms: i64) -> bool {
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
    pub(super) fn probe_run_summary(&mut self, now_ms: i64) {
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

    /// Feed a market-data event to the engine; applies any trend-break bid
    /// cancellation and, for round updates, nothing else.
    pub fn engine_on_data(&mut self, ev: crate::engine::DataEvent, now_ms: i64) {
        // #351: advance in-flight taker legs BEFORE the incoming event is
        // mirrored into `self.books`, so a leg whose `execute_at_ms` falls at
        // this clock is matched against the book as the venue had it — never
        // against the event that arrives at (or after) the same instant. The
        // tick path drains too (rounds with no data still advance time).
        self.drain_inflight_takers(now_ms);
        // Archive the raw event BEFORE anything consumes it, so a replay sees
        // exactly the stream the engine saw (P-1.3). The event's own timestamp is
        // authoritative (it is the clock the engine decides on); the caller's
        // `now_ms` is one step behind, a second drain here is a no-op
        // (legs in stage 2 wait for `report_at_ms`).
        if let Some(a) = self.event_archive.as_mut() {
            let own = crate::data_source::event_at_ms(&ev);
            let at = if own > 0 { own } else { now_ms };
            a.record_at(at, &ev);
        }
        match &ev {
            crate::engine::DataEvent::Book { token_id, .. } => {
                self.stats.books += 1;
                // 探针（任务二 1-3）：一次行情 tick。venue 拿得到就带，
                // 拿不到记空标签（untagged）。纯内存 insert——回放性能
                // 影响可忽略，且不改变任何既有语义。
                let venue = self.venue_of_token(token_id);
                self.probe_ticks.note_tick(token_id, &venue, now_ms);
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            // 盘口批次原子化：批次是 N 本书的合体 —— 每本各计一次 books（与
            // 逐事件路径同口径），一次唤醒（引擎在批次后只应评估一次）。
            crate::engine::DataEvent::Books { books, .. } => {
                self.stats.books += books.len() as u64;
                // 探针（任务二 1-3）：批内每本都是一次行情 tick（与逐事件
                // 路径同口径）。
                for (token, _, _) in books {
                    let venue = self.venue_of_token(token);
                    self.probe_ticks.note_tick(token, &venue, now_ms);
                }
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            crate::engine::DataEvent::TopOfBook { token_id, .. } => {
                self.stats.tops += 1;
                let venue = self.venue_of_token(token_id);
                self.probe_ticks.note_tick(token_id, &venue, now_ms);
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            crate::engine::DataEvent::Spot { .. } => {
                self.stats.spots += 1;
                // 探针：spot 源仍在更新（信号失效监控的"活着"信号）；
                // spot 也算数据——spot-only 部署的停更监控才有起点。
                self.probe_spot.note(now_ms);
                self.probe_ticks.note_any_data(now_ms);
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
            }
            // #352: observed on-chain prints / round boundaries are archived
            // above but are not engine inputs — no counter, no strategy effect.
            crate::engine::DataEvent::Trade { .. } => {}
            crate::engine::DataEvent::RoundEnd { .. } => {}
            // #377 (治 #2): a verdict from the stream settles its market
            // directly — recorded like any other data event (above), counted,
            // and handed to the settlement book. A fill-level wake keeps the
            // fast path's evaluator in step with the position-set change the
            // close causes.
            crate::engine::DataEvent::Resolution {
                condition_id,
                payouts,
                neg_risk,
                ..
            } => {
                self.stats.resolutions += 1;
                if self.replay_sched() {
                    self.fast_eval_wake = true;
                }
                self.on_market_resolution(
                    blitzkrieg_market_api::MarketResolution {
                        condition_id: condition_id.clone(),
                        resolved: true,
                        payouts: payouts.clone(),
                        neg_risk: *neg_risk,
                        // The converter recovered this verdict from the
                        // wallet's own redemption activity; naming the source
                        // keeps the settlement journal's provenance honest.
                        source: "archive".to_string(),
                    },
                    now_ms,
                );
            }
            // #388: a wallet-level cash row credits the active account's
            // ledger at the row's own timestamp — money the venue paid
            // outside any fill. NOTHING else moves: no position, order or
            // settlement semantics read it, so a corpus without these rows
            // behaves exactly as before (the branch itself is the delta).
            crate::engine::DataEvent::Cashflow {
                which, amount_usd, ..
            } => {
                match which {
                    crate::engine::CashflowKind::Rebate => {
                        self.stats.cashflow_rebates += 1;
                        self.stats.cashflow_rebates_usd += *amount_usd;
                    }
                    crate::engine::CashflowKind::Reward => {
                        self.stats.cashflow_rewards += 1;
                        self.stats.cashflow_rewards_usd += *amount_usd;
                    }
                }
                self.accounts
                    .active_ledger_mut()
                    .credit_cashflow(*amount_usd, which.as_str());
                // ponytail: the row names no account — DataEvent::Cashflow
                // carries none (#388) and inventing one would mis-attribute.
                // The venue wallet is a process-level funder fact; the credit
                // stays on the active book, disclosed here.
            }
            // #427: a confirmed cross-venue mapping rides the stream as
            // strategy-layer state. The core counts it (corpus completeness)
            // and stores it for the read view; nothing in the trading path
            // acts on it, so a corpus without these rows is byte-identical.
            crate::engine::DataEvent::UnifiedMapping { event, .. } => {
                self.stats.unified_mappings += 1;
                if let Some(e) = self.engine.as_mut() {
                    e.put_unified_mapping(event.clone());
                }
            }
            crate::engine::DataEvent::RoundMarkets { markets, .. } => {
                // #498: a same-slot second feed is a STAGGERED-LISTING top-up
                // (the engine merges it into the scanner without any rollover),
                // so the rollover bookkeeping below must not run for it: it
                // would kill in-flight taker legs whose token the top-up's
                // partial list doesn't carry, sweep live exit reasons and wake
                // the replay fast path for a round that never changed. The
                // predicate mirrors the engine's VENUE-BLIND slot membership
                // (`.first()` generalized to `.any()`): a same-slot feed from
                // ANY venue in a mixed table is the same staggered-listing
                // top-up. Single-venue tables hold one slot, so `any` == the
                // old `first` check — #498's corpus stays byte-identical.
                // The event still reaches `engine.on_data` below — only THIS
                // arm's extras skip.
                let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
                let venue = markets
                    .first()
                    .map(|m| m.venue.as_str())
                    .unwrap_or_default()
                    .to_string();
                // B1c/E2: slot numbers are only comparable WITHIN one duration
                // family — the membership test is the (slot, duration) PAIR the
                // engine's merge arm (engine.rs) already judges, not the bare
                // slot. The feeding batch's own declared `round_duration_sec`
                // (0 = legacy archive → the configured global grid) must match
                // the resident row's, or a foreign-family batch's first feed is
                // misread as a rollover and the kill arm below (scoped by the
                // feeding VENUE alone, pre-E2) murders a sibling family's
                // in-flight taker on the same venue. Single-family deployments
                // resolve every row and every batch to the SAME duration, so
                // the added condition is constant-true there — the verdict is
                // bit-identical with the pre-B1c slot-only check. The kill arm
                // below gains the same family scope: an in-flight leg dies only
                // when its OWN family rolled over.
                let global_duration_sec = self.config.round_duration_sec;
                let feed_duration = crate::engine::row_family_duration(
                    markets.first().map(|m| m.round_duration_sec).unwrap_or(0),
                    global_duration_sec,
                );
                let top_up = slot != 0
                    && self.engine.as_ref().is_some_and(|e| {
                        e.scanner().markets().iter().any(|m| {
                            m.round_slot == slot
                                && crate::engine::row_family_duration(
                                    m.round_duration_sec,
                                    global_duration_sec,
                                ) == feed_duration
                        })
                    });
                if !top_up {
                    self.stats.rounds += 1;
                    // RFC 568 阶段一：轮次推进 ⇒ 旧轮配对闭账（retire_before，
                    // PairBook 的清退臂 —— Closed 终态吸收迟到的迟发事件）。
                    self.pair_retire_before(slot);
                    // 探针：新一轮的 token 覆盖掉上一轮的——per_token 表键随
                    // 轮替换保持有界（旧 token 不再被汇总点名）。round 声明
                    // 也算数据（零 tick 探针的起点）。
                    self.probe_ticks.per_token.clear();
                    self.probe_ticks.broken.clear();
                    self.probe_ticks.note_any_data(now_ms);
                    // ④ 统一映射：新的一轮带来新市场 ⇒ 重新提议跨所配对候选
                    // （只提议不 confirm）。top-up 跳过：#498 增量合并不换轮，
                    // 旧候选仍然有效，重复刷只是日志噪声。
                    self.propose_unified_candidates(markets, now_ms);
                    // #377 (治 #2): record which markets the archive promised a
                    // verdict for. Their dry-ladder queries are suppressed below —
                    // the stream settles them, and a query stamped here would wait
                    // for an answer that never comes (blind-alert false positive).
                    for m in markets {
                        if m.archive_verdict {
                            self.archive_verdicts.insert(m.condition_id.clone());
                        }
                    }
                    // Replay fast path: a rollover replaces the scanner's market
                    // list and restarts the round clock — the next evaluation sees
                    // a different round context, and the timing verdict flips.
                    if self.replay_sched() {
                        self.fast_eval_wake = true;
                        self.fast_next_timing_ms = now_ms;
                    }
                    // A round rollover replaces the whole round (`engine.markets`
                    // carries the new list), so it is the moment any pending-close
                    // reason whose position is gone can no longer belong to
                    // anything live (issue #190).
                    self.sweep_exit_reasons();
                    // #351: the rollover closed every market of THIS VENUE's old
                    // round (multi-plugin data plane: a Polymarket rollover does
                    // not close Kalshi markets). A taker leg still un-matched for
                    // a token the feeding venue's new round does not carry can
                    // never fill — the venue rejects a FOK for a closed market —
                    // so the honest model kills it NOW (it reports as a rejection
                    // at its report time, reservation released). A leg that
                    // already matched keeps its fill: the venue matched it before
                    // the close.
                    //
                    // E2 (family scope): "this venue's round" means the VENUE'S
                    // FEEDING FAMILY — the engine's rollover replacement arm
                    // (B1c) is already family-scoped (a same-venue sibling
                    // family is replaced by its OWN feed), and this kill arm
                    // must not out-scope it: a 1h rollover on the same venue
                    // must not murder a 15m leg whose round is still running.
                    // The leg's family resolves three-state off its own market
                    // row (own declaration > the global grid) — the same
                    // projection `row_family_duration` gives every other
                    // consumer. Single-family tables hold one duration, so the
                    // added condition is constant-true there: byte-identical
                    // with the pre-E2 venue-only scope.
                    let tokens: Vec<String> = markets
                        .iter()
                        .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                        .collect();
                    for leg in self.inflight_takers.iter_mut() {
                        if leg.matched_price.is_none() && leg.rejected_msg.is_none() {
                            // Only legs on the ROLLED-OVER (venue, family) are in
                            // scope: a sibling venue's leg is not closed by this
                            // feed, and neither is a sibling FAMILY's leg on the
                            // same venue. The leg's token comes from its own OME
                            // order.
                            let leg_token = self
                                .ome
                                .get(&leg.order_id)
                                .map(|o| o.token_id.clone())
                                .unwrap_or_default();
                            let (leg_venue, leg_family) = self
                                .engine
                                .as_ref()
                                .and_then(|e| e.market_for_token(&leg_token))
                                .map(|m| {
                                    (
                                        m.venue.clone(),
                                        crate::engine::row_family_duration(
                                            m.round_duration_sec,
                                            global_duration_sec,
                                        ),
                                    )
                                })
                                .unwrap_or_default();
                            if leg_venue != venue || leg_family != feed_duration {
                                continue;
                            }
                            let ended = self
                                .ome
                                .get(&leg.order_id)
                                .map(|o| !tokens.contains(&o.token_id))
                                .unwrap_or(true);
                            if ended {
                                leg.rejected_msg = Some(format!(
                                    "taker_killed_round_ended: the order's market closed while the leg was in flight (token {})",
                                    self.ome
                                        .get(&leg.order_id)
                                        .map(|o| o.token_id.clone())
                                        .unwrap_or_default()
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Mirror the latest book into `self.books`, which the position/exit path
        // reads (`run_exit_checks`). Without this, a Rust-native feed (--feed-ws)
        // left Core.books empty, so open positions never re-valued and NO
        // non-forced exit (TP/trailing/SL) could ever fire.
        let mirrored_tokens: Vec<(String, i64)> = match &ev {
            crate::engine::DataEvent::Book {
                token_id,
                bids,
                asks,
                now_ms: event_ms,
            } => {
                self.books.insert(
                    token_id.clone(),
                    crate::sim::Book {
                        bids: bids.clone(),
                        asks: asks.clone(),
                        // #462: keep the FEED's receive time — the freshness
                        // gates downstream must age THIS book by when the
                        // venue last spoke, not by the host's tick clock.
                        received_ms: *event_ms,
                    },
                );
                vec![(token_id.clone(), *event_ms)]
            }
            // 盘口批次原子化：整批按事件顺序逐本镜像（每本自己的批次时间戳
            // `received_ms`）。镜像本身无撕裂可言 —— 它只喂 exit 的 mark，
            // 不喂策略评估；撕裂的根在策略读的那一面，由下面的 Batch 应用
            // 顺序消除。这里借用已结束，循环里的本地副本随取随用。
            crate::engine::DataEvent::Books {
                books,
                now_ms: event_ms,
            } => {
                for (token_id, bids, asks) in books {
                    self.books.insert(
                        token_id.clone(),
                        crate::sim::Book {
                            bids: bids.clone(),
                            asks: asks.clone(),
                            received_ms: *event_ms,
                        },
                    );
                }
                books
                    .iter()
                    .map(|(t, _, _)| (t.clone(), *event_ms))
                    .collect()
            }
            crate::engine::DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                now_ms: event_ms,
            } => {
                let mut bids = Vec::new();
                let mut asks = Vec::new();
                if let Some(b) = best_bid
                    && *b > Decimal::ZERO
                {
                    bids.push((*b, Decimal::ONE));
                }
                if let Some(a) = best_ask
                    && *a > Decimal::ZERO
                {
                    asks.push((*a, Decimal::ONE));
                }
                // Only overwrite when we actually have a side (don't clobber a
                // full book with an empty top-of-book update).
                if !bids.is_empty() || !asks.is_empty() {
                    self.books.insert(
                        token_id.clone(),
                        crate::sim::Book {
                            bids,
                            asks,
                            received_ms: *event_ms,
                        },
                    );
                    vec![(token_id.clone(), *event_ms)]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };

        // Replay fast path: the mirrored book is the ONLY input a position's
        // marks can move on between events, so record the token as dirty.
        // Consumed (and cleared) by the next maintenance tick. #390: the
        // nofast replay needs the same wake — its ladder runs on deadlines
        // plus dirty tokens, so a book event must reach the next tick's pass
        // there too. The set is a pure dirty marker; both paths consume it.
        for (token, _) in &mirrored_tokens {
            self.fast_dirty_tokens.insert(token.clone());
        }

        // KI-1: the market path must also settle resting maker orders the way
        // `book_snapshot` does — a crossing feed event is exactly when a
        // dry maker fill would happen at the venue, and skipping it made every
        // dry entry escalate to taker (a systematically more expensive economy).
        if self.config.mode.settles_locally() {
            for (token, _) in &mirrored_tokens {
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.token_id == *token && rests_on_book(o.mode))
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    self.try_maker_fill(&id, now_ms);
                }
            }
        }

        // Shadow Evolution observation. D-3 fidelity: the shadow is fed AFTER the
        // live engine consumes this very event, so a variant replays the SAME tick
        // instant — same book and same trend confirmation — the live strategy
        // decided on. Feeding it before `engine.on_data` (the old behaviour) gave
        // every variant the PREVIOUS book, a one-tick lag that mis-timed entries
        // and diluted the counterfactual. Capture what we need before `ev` moves.
        enum ShadowFeed {
            Book(String, i64),
            /// 盘口批次原子化：一个批次的整组 (token, 批次时间戳)。双胞胎按
            /// 批次逐本喂入（同一时间戳），与 live 的 Batch 应用保持一致。
            Books(Vec<(String, i64)>),
            Round(Vec<CryptoMarket>, i64),
        }
        let shadow_feed = if self.shadow_evolution.is_enabled() {
            match &ev {
                crate::engine::DataEvent::Book {
                    token_id,
                    now_ms: t,
                    ..
                }
                | crate::engine::DataEvent::TopOfBook {
                    token_id,
                    now_ms: t,
                    ..
                } => Some(ShadowFeed::Book(token_id.clone(), *t)),
                crate::engine::DataEvent::Books { books, now_ms: t } => Some(ShadowFeed::Books(
                    books.iter().map(|(t2, _, _)| (t2.clone(), *t)).collect(),
                )),
                crate::engine::DataEvent::RoundMarkets { markets, now_ms: t } => {
                    Some(ShadowFeed::Round(markets.clone(), *t))
                }
                _ => None,
            }
        } else {
            None
        };

        // Drive the live engine (when attached). Absence must not skip the shadow
        // round feed below, so this is an `if let`, not an early return.
        if let Some(engine) = self.engine.as_mut() {
            let broken = engine.on_data(ev);
            // E29 (§12.2): CLOSED bars push immediately — NEVER throttled (a
            // swallowed close would leave the chart missing that bar forever).
            // The aggregator yields each closed bar exactly once, so the
            // "一 bar 一条，一条不少" promise is this loop alone. Drained here,
            // while `engine` is borrowed — the emits below run after the
            // borrow ends (NLL), past the cancel loop.
            let closed_bars = engine.take_closed_klines();
            for (token, _price) in broken {
                // Regime change: pull resting entry bids for this token.
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.side == Side::Buy && o.token_id == token)
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    let _ = self.cancel(&id, now_ms);
                }
            }
            for bar in closed_bars {
                self.emit(Event::KlineUpdate { kline: bar });
            }
        }

        // Now feed the shadow the tick the live engine just consumed.
        match shadow_feed {
            Some(ShadowFeed::Book(token_id, t)) => {
                let book = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.book_snapshot(&token_id));
                if let Some(book) = book {
                    self.shadow_evolution_on_tick(&token_id, &book, t);
                }
            }
            Some(ShadowFeed::Books(books)) => {
                // 同一批次逐本喂给双胞胎：双胞胎的 on_book 是逐本观测的
                // （与 live 策略相同），整批共享一个时间戳。
                for (token_id, t) in books {
                    let book = self
                        .engine
                        .as_ref()
                        .and_then(|e| e.book_snapshot(&token_id));
                    if let Some(book) = book {
                        self.shadow_evolution_on_tick(&token_id, &book, t);
                    }
                }
            }
            Some(ShadowFeed::Round(markets, t)) => {
                // Seed the twins with the same opening mids the live engine replays
                // for this round (engine.rs RoundMarkets), so confirmation clocks
                // start together instead of one book late.
                let tokens: Vec<String> = markets
                    .iter()
                    .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    .collect();
                let seeds: Vec<(String, crate::model::OrderbookSnapshot)> = self
                    .engine
                    .as_ref()
                    .map(|e| {
                        tokens
                            .into_iter()
                            .filter_map(|t| e.book_snapshot(&t).map(|b| (t, b)))
                            .collect()
                    })
                    .unwrap_or_default();
                self.shadow_evolution_on_round(&markets, &seeds, t)
            }
            None => {}
        }
    }

    /// 盘口批次原子化：批式拉取（polymarket `POST /books` 一类）的整批 book
    /// 一次性入核 —— 调用方必须已经持有 Core 锁（或本调用就是唯一持锁点），
    /// 批内所有 book 在同一次持锁区间里完成镜像 + 引擎应用，维护/评估 tick
    /// 不可能插进来读到「一半新一半旧」的撕裂状态。
    ///
    /// 与 [`Self::engine_on_data`] 的差异只有两点：归档为一条 `k:"books"` 批
    /// 行（新增的 kind，旧语料没有它、逐事件回放基线不受影响）；无
    /// `DataEvent` 归档/影子复用 —— 引擎应用走 [`Engine::on_data_books`]，
    /// 每本书与单事件路径同一段 apply/notify 代码。返回被趋势断裂命中的
    /// token 上的 resting 买单（已撤），供调用方记账。
    ///
    /// #548：批路径曾对归档与探针双盲 —— 归档永远没有 `k:"books"` 行
    /// （编码器在 data_source.rs，无人喂）、探针记不到批内任何 tick
    /// （zero-tick 名单全部假警、stream break 对批式 venue 永久失明）。
    /// 两写点对齐 `engine_on_data` 的 Book/Books 臂：先归档（回放所见 =
    /// 引擎所见），再在镜像前逐本 note_tick（与单事件路径同口径）。
    pub fn engine_on_books(&mut self, books: Vec<BatchBook>, ts_ms: i64) {
        // #351：与 engine_on_data 同序 —— 先推进在途 taker 腿，再让本批书
        // 落进引擎（腿按「事件到达时」的盘口撮合，永远不与本批同瞬）。
        self.drain_inflight_takers(ts_ms);

        // #548：归档整批为一条 `k:"books"` 批行 —— doc-comment 一直这么
        // 承诺，实现却没写。事件自己的时间戳是权威（engine_on_data 同规）。
        if let Some(a) = self.event_archive.as_mut() {
            a.record_at(
                ts_ms,
                &crate::engine::DataEvent::Books {
                    books: books.clone(),
                    now_ms: ts_ms,
                },
            );
        }

        // #548：批内每本都是一次行情 tick（与 engine_on_data 的 Books 臂
        // 同口径）。归档/探针都只读 books 的形状，镜像写入在两者之后。
        for (token, _, _) in &books {
            let venue = self.venue_of_token(token);
            self.probe_ticks.note_tick(token, &venue, ts_ms);
        }

        // 镜像（exit/mark 路径）：与 Batch 事件同语义 —— 每本 received_ms
        // 统一为批次时间戳。
        for (token_id, bids, asks) in &books {
            self.books.insert(
                token_id.clone(),
                crate::sim::Book {
                    bids: bids.clone(),
                    asks: asks.clone(),
                    received_ms: ts_ms,
                },
            );
        }
        for (token_id, _, _) in &books {
            self.fast_dirty_tokens.insert(token_id.clone());
        }
        // KI-1：与 engine_on_data 同规则 —— 本地结算内核在盘口事件落地时
        // 结算 resting maker 单（逐 token，跨整批）。
        if self.config.mode.settles_locally() {
            for (token_id, _, _) in &books {
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.token_id == *token_id && rests_on_book(o.mode))
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    self.try_maker_fill(&id, ts_ms);
                }
            }
        }

        // 引擎：整批一次应用。撕裂的根在逐 token 的两次独立应用之间 ——
        // 这里一次调用顺序应用整批，策略观测永远对齐同一批次快照。
        if let Some(engine) = self.engine.as_mut() {
            let broken = engine.on_data_books(&books, ts_ms);
            let closed_bars = engine.take_closed_klines();
            for (token, _price) in broken {
                // Regime change: pull resting entry bids for this token.
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.side == Side::Buy && o.token_id == token)
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    let _ = self.cancel(&id, ts_ms);
                }
            }
            for bar in closed_bars {
                self.emit(Event::KlineUpdate { kline: bar });
            }
        }

        // 影子演化：与 live 同一批次逐本喂入（时间戳统一）。
        if self.shadow_evolution.is_enabled() {
            for (token_id, _, _) in &books {
                let book = self.engine.as_ref().and_then(|e| e.book_snapshot(token_id));
                if let Some(book) = book {
                    self.shadow_evolution_on_tick(token_id, &book, ts_ms);
                }
            }
        }

        // 回放唤醒：批次是引擎输入，下一拍要真评估（与 Book 事件同效果，
        // 计数在 stats match 外由调用方 on_books 完成 —— 这里补计数）。
        self.stats.books += books.len() as u64;
        if self.replay_sched() {
            self.fast_eval_wake = true;
        }
    }

    /// Run one evaluation cycle: engine produces entry orders, core places them.
    /// Entries go through the usual risk/capacity gates; rejections are skipped.
    /// A configured per-strategy cap (P-1.1) is checked before the order layer.
    /// Replay fast path: whether THIS cycle must run a real Lua evaluation —
    /// an engine input arrived since the last one (`fast_eval_wake`), a
    /// round-timing boundary the gates read is due (`fast_next_timing_ms`),
    /// or the fallback window elapsed. Between those, the same inputs
    /// reproduce the same output, so the call is skipped.
    pub(super) fn fast_eval_due(&self, now_ms: i64) -> bool {
        self.fast_eval_wake
            || now_ms >= self.fast_next_timing_ms
            || now_ms - self.fast_last_eval_ms >= FAST_EVAL_FALLBACK_MS
    }

    pub fn engine_evaluate(&mut self, now_ms: i64) -> usize {
        if self.engine.is_none() {
            return 0;
        }
        self.stats.evaluations += 1;
        // Replay fast path: a quiet tick skips the Lua call entirely. Between
        // engine inputs (book/top/spot/rollover — the dirty wake) the only
        // strategy inputs that move are the round clock's integer-second
        // fields, and their gate verdicts cannot flip inside the open
        // interval the boundary wake (`fast_next_timing_ms`) covers; the
        // fallback bounds anything the boundary does not model to 2 s.
        // The evaluations COUNTER still advances every cycle: the sparse-
        // cadence contract (`sparse_stream_evaluates_every_due_cycle`) pins
        // the scheduled cycle count, not the wall cost of one cycle.
        // #390: the nofast replay runs the same gates through
        // `replay_scheduling` — the strategy surface it observes (signals,
        // fills, trades) is identical, and the eval-call health counter is
        // host diagnostic, not replay semantics.
        if self.replay_sched() && !self.shadow_evolution.is_enabled() && !self.fast_eval_due(now_ms)
        {
            // Issue #390 (replay nofast): a QUIET cycle. The base walk ran a
            // full Lua collection here and re-submitted its suggestions into
            // the kernel-side gates; this cycle re-runs the STANDING
            // suggestions of the last real evaluation through the SAME
            // kernel-side pipeline instead — the gate/counter/log/placement
            // stream is reproduced cycle for cycle without the Lua call
            // (see `engine_replay_cycle`). A stable-phase cache answers with
            // orders; an unstable one (a phase transition the last real
            // evaluation introduced — a latch fired, an emission consumed)
            // refuses, and THIS cycle runs a real evaluation: the base's
            // next collection would collect the post-transition output.
            // The wake flags stay armed either way: a quiet cycle consumes
            // neither the input wake nor the timing boundary — both belong
            // to the next REAL evaluation.
            if let Some(orders) = self.engine_replay_cycle(now_ms) {
                return orders;
            }
        }
        if self.replay_sched() {
            // A real evaluation consumes the wake; the timing boundary is
            // recomputed after the engine borrow ends (below).
            self.fast_eval_wake = false;
            self.fast_last_eval_ms = now_ms;
            self.fast_next_timing_ms = i64::MAX;
        }
        // Shadow Evolution: evaluate FIRST so any applied hot-swap is in force
        // for this very cycle's order decision (next-tick semantics, no restart).
        self.shadow_evolution_evaluate(now_ms);
        // E13: the 72h DEEP round rides the same cadence, so a long-running
        // process re-anchors its variant sets (compound mutants) without an
        // operator. The check itself is one comparison per cycle.
        self.shadow_evolution_maybe_cycle(now_ms);
        // #390: read before the engine borrow opens (the re-arm below uses it
        // while `engine` is still mutably borrowed).
        let replay_sched = self.replay_sched();
        // The position snapshot strategies see via `bk.holdings()` — built
        // before the engine borrow opens (same once-per-cycle cadence as the
        // equity push below).
        let held_view = self.held_position_view();
        // RFC 568 阶段一：本周期每策略的配对簿视图快照（owner 过滤在 Core
        // 侧做完 —— 实例主账户 + arb 腿账户白名单，与 holdings 视图拓宽同
        // 形）。也在引擎借用打开前构建；策略层经 `bk.arb.pairs()` 只读
        // （最终一致，附注 2）。
        let pair_views: Vec<crate::pair::PairView> = self
            .engine
            .as_ref()
            .map(|e| {
                e.hosted()
                    .iter()
                    .filter(|s| s.enabled)
                    .map(|s| {
                        let mut accounts: HashSet<String> = HashSet::new();
                        accounts.insert(s.account_id.clone());
                        if let Some(ls) = s.strategy.arb_leg_accounts() {
                            accounts.extend(ls.iter().cloned());
                        }
                        (s.strategy.name().to_string(), accounts)
                    })
                    .collect::<Vec<(String, HashSet<String>)>>()
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(name, accounts)| self.pair_views_for(&name, &accounts))
            .collect();
        let engine = self.engine.as_mut().expect("engine present");
        // #202: the equity-relative sizing is a percentage of the account, and
        // the account is the ledger — pushed here, once per cycle, so a ticket
        // can never be sized against a balance the process no longer has.
        // E28: the legacy scalar mirrors the ACTIVE account's book; the
        // per-scope pushes below carry every configured account's own book,
        // trading switch, and venue binding, so each scope sizes/gates
        // against ITS account. (§9.4: the kernel drives strategies; per-
        // account routing happens when each order passes through the gates.)
        engine.set_equity_usd(self.accounts.active_ledger().balance());
        for a in self.accounts.account_list() {
            let id = a.id.as_str();
            let equity = self
                .accounts
                .ledger_of(&a.id)
                .map(|l| l.balance())
                .unwrap_or_default();
            engine.set_account_equity(id, equity);
            engine.set_account_trading(id, a.trading_enabled);
            engine.set_account_venue(id, &a.venue);
            engine.set_account_frozen(id, self.account_freezes.contains_key(id));
        }
        engine.set_positions_view(held_view);
        // 视图落引擎的 per-cycle 快照（evaluate 的 ctx 构造从这取）。
        engine.set_pair_views_snapshot(pair_views);
        let orders = engine.evaluate(now_ms);
        // Replay fast path: recompute when the round-timing verdict can next
        // flip — quiet ticks skip the Lua call until then (or the fallback
        // beat, or a new engine input). #390: nofast replay re-arms the same
        // boundary. (`replay_sched` was read before the engine borrow opened.)
        if replay_sched {
            self.fast_next_timing_ms = engine.next_timing_boundary_ms(now_ms).unwrap_or(i64::MAX);
        }
        // E30 (§6.3): drain the sandbox poison alerts now; each is raised as
        // ONE RISK_ALERT after the engine borrow ends below — the message
        // names the strategy, the sandbox itself refuses every later call
        // from it, and the kernel keeps running (a poisoned strategy is
        // quarantined, not a crash).
        // #363: the round timing this cycle was judged against, copied out
        // while the engine borrow is alive so the policy facts below do not
        // extend it (the engine reference must end before `self.emit`).
        let policy_time_left_sec = engine.last_time_left_sec();
        let poison_alerts = engine.drain_poison_alerts();
        for (name, alert) in &poison_alerts {
            tracing::error!(strategy = %name, %alert, "lua strategy poisoned; all further calls refused");
        }
        // Close intents produced by strategies this cycle join the SAME exit
        // submission path as policy exits (handled in run_exit_checks).
        self.strategy_exits.extend(engine.drain_strategy_exits());
        engine.tally_blocked();
        // E2-b: an honoured gate exemption is always logged (and counted), so an
        // opt-out is auditable rather than a silent hole in the entry gates.
        for rec in engine.take_exemptions() {
            tracing::info!(target: "strategy", "{}", rec.audit_line());
        }
        // RFC 568 阶段一：A≠B 拒绝回执与配对声明都在引擎借用内先收下来
        // （回执 → APPLICATION 事件、声明 → precheck 过闸后 declare，都在
        // 借用结束后 —— 与 poison_alerts 的先收后发同一模式）。
        let pair_refusals = engine.take_pair_refusals();
        let pair_declarations = engine.take_pair_declarations();
        for (name, alert) in poison_alerts {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!("lua strategy '{name}' poisoned: {alert}"),
            });
        }
        self.stats.signals += orders.len() as u64;
        let mut tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        // ── RFC 568 阶段一（补钉 5 的消费侧）：A≠B 校验的拒绝回执 →
        // APPLICATION 错误事件（点名字段 pair.key）。订单路径照旧 —— 校验
        // 拒的是「声明」，不是腿（裸腿由 arb_precheck 的整组语义与内核去重
        // 兜底）；声明被拒 ⇒ 该腿不进 PairBook。
        for reason in pair_refusals {
            self.emit(Event::Error {
                error: CoreError::new(CoreErrorCode::InvalidParams, reason),
            });
        }
        // ── E-VA: the atomic-batch pre-reservation, judged BEFORE the first
        // leg's gate runs — the whole-cycle dimensions (credentials/scope,
        // RFC #541 hook B) plus the per-account pot dimension. Strategies
        // that declared `atomic_batch` (and sit in `virtual_limits`) reserve
        // their WHOLE batch against the virtual pot: an over-cap batch is
        // cancelled wholesale — zero legs placed, zero real reservations
        // taken — with the refusal bucketed through the existing
        // StrategyLimit accounting. Everything else (every shipped strategy,
        // every undeclared batch) finds no governed scope and skips
        // untouched.
        self.arb_precheck_filter(&mut tokens);
        // ── RFC 568 阶段一（补钉 3 的落点）：声明受理在 arb_precheck 过闸
        // 之后 —— 预检拒整批 ⇒ 不入账（凭证维度由预检管，声明不重复校验）；
        // 过闸 ⇒ 逐腿 declare 进 PairBook。存活判定直接用 precheck 后的
        // 幸存单集（strategy, 落账账户, token）三元组唯一对应一条腿：scope
        // 整组丢、策略丢、去重吞掉的声明都不入账（配对簿只记将要发生的事
        // 实的配对骨架）；经济身份 (asset, slot) 在 pair_declare_leg 里从腿
        // token 经 scanner 派生。
        let survived: HashSet<(String, String, String)> = tokens
            .iter()
            .map(|(_, req)| {
                (
                    req.strategy.clone(),
                    req.account_id.to_string(),
                    req.token_id.clone(),
                )
            })
            .collect();
        for d in pair_declarations {
            if !survived.contains(&(
                d.owner_strategy.clone(),
                d.leg_account.clone(),
                d.token_id.clone(),
            )) {
                continue;
            }
            self.pair_declare_leg(&d, now_ms);
        }
        let mut placed = 0;
        // ── #363: the execution policy's account-level verdict, judged BEFORE
        // arbitration. Two decisions ride here:
        //
        //   1. `Skip` (a rule hit or the equity floor) — the intent never
        //      reaches `place()`, so it never reaches the RiskGate either;
        //      the refusal is bucketed `policy.skip` and audited with the
        //      rule's own reason. This is the ONLY place a policy verdict
        //      removes an order: everything downstream (RiskGate, breakers,
        //      capacity, reserve, OME) runs untouched — "规则不能绕过
        //      RiskEngine" holds because the policy can only REMOVE an
        //      intent, never relax a gate or resize past the risk cap.
        //   2. `Place { budget_usd }` — the budget is an UPPER BOUND: the
        //      request's own share band (min_shares/max_shares, engine
        //      sizing) still decides the ticket, and the policy shrinks it
        //      when the shares cost more than the budget allows. With the
        //      shipped defaults (ratio 0.10, floor 1, ceiling 50) the bound
        //      sits at or above every ticket the share band can produce, so
        //      no order changes size — exit-economics parity by construction.
        let policy_verdicts: HashMap<
            String,
            Result<crate::execution_policy::PolicyOutput, crate::execution_policy::PolicyRefusal>,
        > = if self.execution_policy.is_some() {
            // The open_positions fact counts LIVE positions only — an expired
            // round's stuck position is settlement-dead (same rule as
            // PositionManager::can_open) and must not trip the shipped
            // `open_positions >= N` backstop.
            let open = self.positions.open_positions();
            let open_for = |acct: &str| {
                open.iter()
                    .filter(|p| p.account_id.as_str() == acct && p.expires_at_ms > now_ms)
                    .count() as u32
            };
            tokens
                .iter()
                .map(|(_, req)| {
                    let facts = crate::execution_policy::PolicyInput {
                        available_balance: self
                            .accounts
                            .ledger_of(&req.account_id)
                            .map(|l| l.balance())
                            .unwrap_or_default(),
                        total_equity: self
                            .accounts
                            .ledger_of(&req.account_id)
                            .map(|l| l.balance())
                            .unwrap_or_default(),
                        open_positions: open_for(req.account_id.as_str()),
                        current_price: req.price,
                        time_left_sec: policy_time_left_sec,
                        symbol: req.asset.clone(),
                        recent_pnl_1h: self
                            .positions
                            .recent_pnl_1h_for(req.account_id.as_str(), now_ms),
                        consecutive_losses: self.breaker.consecutive_losses(&req.strategy),
                    };
                    let verdict = crate::execution_policy::evaluate(
                        self.execution_policy.as_ref().expect("checked above"),
                        req.account_id.as_str(),
                        &facts,
                    );
                    (req.account_id.to_string() + ":" + &req.token_id, verdict)
                })
                .collect()
        } else {
            HashMap::new()
        };
        // ── v0.3 Wave 0 (#329): the E25 arbitration delegation seam ──────────
        // E25 inserts its one-line per-intent delegation at the head of the
        // loop below: `process_intent` (arbitration/) runs BEFORE the E16
        // portfolio cap, so every strategy intent is arbitrated exactly once
        // and the SAME Request that cleared the gates is the one handed to
        // `self.place(...)`. Wave 0 lands the seam only — a comment, not a
        // call: `arbitration/` does not exist yet and this PR is
        // behavior-free by charter (deviation disclosed in the PR body).
        for (token, mut req) in tokens {
            let name = req.strategy.clone();
            // ── #406: the zero/negative-price fail-closed guard — the FIRST
            // placement gate, before the policy arm. A strategy that names a
            // price <= 0 on an entry is a malformed intent (a prediction
            // market has no legal quote at or below zero), and every price
            // the ticket touches downstream DIVIDES by it: the policy's
            // budget shrink (`budget_usd / req.price`, service.rs Place arm)
            // is the earliest such division, and a zero price PANICKED the
            // process there (rust_decimal Division by zero) before the
            // arbitration Gate 1 price-band check — which would have refused
            // this intent — ever ran. Fail closed instead: the entry is
            // refused with its own counter and cause bucket, the process
            // keeps running, and the audit trail names the strategy.
            // Closing intents are exempt: an exit prices off the book and
            // position state, never off the strategy's entry quote, and a
            // held position must always be closable.
            if req.side == crate::model::Side::Buy && req.price <= Decimal::ZERO {
                tracing::error!(target: "strategy",
                    "entry rejected: strategy={name} cause=illegal.price token={} price={} — \
                     non-positive entry price is refused fail-closed (#406): it would divide \
                     by zero in sizing/policy/physics",
                    req.token_id, req.price);
                self.stats.place_rejected += 1;
                self.stats.illegal_price_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                *acc.rejection_causes
                    .entry("illegal.price".to_string())
                    .or_default() += 1;
                continue;
            }
            // ── #363: account-level cooldown gate. A rule that returned
            // `Cooldown` last cycle parked the ACCOUNT here for N seconds:
            // every entry this account names is refused until the clock runs
            // out (the clock is the caller's — the rule states a duration,
            // the kernel owns the time). Expired entries are dropped on
            // sight, and nothing about exits is slowed: only entries.
            if let Some(&until) = self.policy_cooldowns.get(req.account_id.as_str()) {
                if until > now_ms {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.cooldown account={} \
                         {}s left", req.account_id, (until - now_ms + 999) / 1000);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                self.policy_cooldowns.remove(req.account_id.as_str());
            }
            // ── #363: the policy verdict for THIS intent (judged above, before
            // any gate). A Skip refuses here — the intent never reaches the
            // RiskGate, which is the only way a policy may touch an order: it
            // removes, never relaxes. A Place caps the ticket size.
            match policy_verdicts
                .get(&format!("{}:{}", req.account_id, req.token_id))
                .map(|v| v.as_ref())
            {
                Some(Ok(crate::execution_policy::PolicyOutput::Skip { reason })) => {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.skip reason={reason}");
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Cooldown { seconds })) => {
                    let until = now_ms + seconds * 1000;
                    self.policy_cooldowns
                        .insert(req.account_id.to_string(), until);
                    tracing::info!(target: "strategy",
                        "policy cooldown: account={} {}s (entries resume at {})",
                        req.account_id, seconds, until);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Place { budget_usd, .. })) => {
                    // The budget is an UPPER BOUND on the ticket, not a
                    // replacement for the sizing path: the engine's share band
                    // already decided `req.size`, and the policy only shrinks it
                    // when the shares cost more than the account's budget allows.
                    // With the shipped defaults the bound never bites (see the
                    // comment at the verdict map), so no existing order changes.
                    let shares = (budget_usd / req.price).floor();
                    if shares >= Decimal::ONE && req.size > shares {
                        tracing::info!(target: "strategy",
                            "policy budget shrink: strategy={} account={} {} -> {shares} shares \
                             (budget {budget_usd} USD)",
                            name, req.account_id, req.size);
                        req.size = shares;
                    }
                    // A budget that cannot buy one whole share emits nothing:
                    // a 0-size order would only turn the refusal into a venue
                    // rejection (the #202 lesson, applied to the policy path).
                    if shares < Decimal::ONE {
                        tracing::info!(target: "strategy",
                            "entry rejected: strategy={name} cause=policy.skip reason=\
                             budget {budget_usd} USD cannot buy one share at {}",
                            req.price);
                        self.stats.place_rejected += 1;
                        let acc = self
                            .strategy_accounting
                            .entry(strategy_acc_key(&name, req.account_id.as_str()))
                            .or_default();
                        acc.rejected += 1;
                        *acc.rejection_causes
                            .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                            .or_default() += 1;
                        continue;
                    }
                }
                Some(Err(refusal)) => {
                    // "account_id 缺失 → 拒绝下单": the policy could not judge
                    // this intent at all. Refuse loudly, never fall through to
                    // defaults.
                    tracing::warn!(target: "strategy",
                        "entry refused: strategy={name} account={} policy refusal: {}",
                        req.account_id, refusal.reason);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                None => {}
            }
            // ── E25 (#331): arbitration BEFORE the E16 portfolio cap — this is
            // the seam Wave 0 planted. Every strategy intent is arbitrated
            // exactly once through the four gates; a Rejected suggestion never
            // reaches place()/OME, an Approved one is handed to the SAME path
            // as before. The gates reuse the existing implementations
            // (OrderIntent::validate / RiskGate / breakers / Ledger::reserve /
            // effective_stop_pct), so no threshold has two truths (§3.3).
            // E28 (§9.2): the intent is arbitrated in the account its own
            // request names — the pipeline's reserve lands in THAT book, not
            // the active one. The posture is judged before the request moves
            // into the intent.
            let account_id = req.account_id.clone();
            let is_close = crate::risk::is_close_intent(&req.internal_key);
            let account_entry_block = self
                .accounts
                .permits_order(&account_id, is_close)
                .err()
                .map(|e| e.message);
            let intent = crate::arbitration::StrategyIntent::from_request(req);
            let outcome = {
                let round_tokens = self
                    .engine
                    .as_ref()
                    .map(|e| e.round_token_ids())
                    .unwrap_or_default();
                let time_left_sec = self
                    .engine
                    .as_ref()
                    .map(|e| e.last_time_left_sec())
                    .unwrap_or(0);
                // E26 (§4.2) + #461: the systemic aggregates are read BEFORE
                // the account borrow — resting BUY notional joins the
                // exposure (cash already promised).
                let systemic_facts = self.systemic_facts(&intent.request.asset, now_ms);
                // The PHYSICS projection needs the strategy's own doctrine:
                // a hold-to-settlement declaration widens the bound stop
                // (the wide containment width, not the 12% ladder).
                let strategy_holds_to_settlement = self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.strategy_holds_to_settlement(&intent.request.strategy));
                // E28 (§9.3): the pipeline's reserve happens in THIS account's
                // book. An unknown account is refused — no implicit empty book,
                // ever (reverse acceptance C) — with the same Gate-2 accounting
                // as an arbitration rejection.
                match self.accounts.get_mut(&account_id) {
                    Err(e) => crate::arbitration::Outcome {
                        decision: crate::arbitration::Decision::Rejected {
                            reason: crate::arbitration::RejectReason::AccountLimit,
                            gate: crate::arbitration::GateId::Risk,
                            detail: e.message,
                        },
                        gates: Vec::new(),
                        latency_us: 0,
                    },
                    Ok(ledger) => {
                        // E28: the #202 equity is a percentage of THIS
                        // account's live balance, same rule as the submission
                        // path.
                        let equity = ledger.balance();
                        let mut ctx = crate::arbitration::IntentCtx {
                            round_tokens: &round_tokens,
                            risk: &self.risk,
                            breaker: &self.breaker,
                            ledger,
                            exit_cfg: &self.config.positions.exit,
                            time_left_sec,
                            now_ms,
                            equity,
                            account_entry_block,
                            systemic_facts,
                            strategy_holds_to_settlement,
                        };
                        crate::arbitration::process_intent(&intent, &mut ctx)
                    }
                }
            };
            self.record_intent_decision(&intent, &outcome, now_ms);
            if let crate::arbitration::Decision::Rejected {
                reason,
                gate,
                detail,
                ..
            } = &outcome.decision
            {
                self.stats.place_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, intent.request.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                // Bucket through the EXISTING classifier over the gate's own
                // message, so the attribution vocabulary is the one place()
                // would have written — the rejection happened earlier, the
                // story an operator reads does not change (E9-c buckets).
                let cause = classify_rejection(&crate::model::CoreError::new(
                    crate::arbitration::reason_code(*reason),
                    detail.clone(),
                ));
                *acc.rejection_causes.entry(cause).or_default() += 1;
                tracing::info!(target: "strategy",
                    "entry rejected: strategy={name} cause=arbitration gate={gate:?} reason={reason:?} detail={detail}");
                continue;
            }
            let mut req = intent.request;
            // E26 (§4.2): a systemic shrink rides the decision as MODIFIED —
            // the audit names the bound that bit; here the reduced size is
            // what gets placed. The pipeline never rewrote the request (the
            // suggestion's provenance stays exactly what the engine emitted),
            // so the shrink is applied at the one point that spends money.
            if let crate::arbitration::Decision::Modified {
                modification: crate::arbitration::Modification::SizeReduced { approved, .. },
                ..
            } = &outcome.decision
            {
                tracing::info!(target: "strategy",
                    "entry modified: strategy={name} systemic shrinks {} -> {approved} shares",
                    req.size);
                req.size = *approved;
            }
            // E16/#98 portfolio-level exposure cap (0 = off): the account's
            // total open commitment across ALL strategies is bounded too —
            // per-strategy caps partition it, but their sum needs its own
            // ceiling. The check ADDS a constraint; the RiskGate hard bounds
            // (kill switch, price band, per-order cap) are untouched.
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit, now_ms)
            {
                // E9-c: the reason existed before but was discarded — bucket
                // it so operator tooling can answer "why did this strategy
                // stop placing?".
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            // E-VA: the batch pre-check passed — HOLD this leg's virtual
            // quota before it spends anything real. A later per-order gate
            // may still refuse the leg: the Err arm below releases its keyed
            // reservation, and a placed leg keeps its hold until the order
            // state machine's terminal transition releases it.
            {
                let account_id = req.account_id.to_string();
                let strategy = req.strategy.clone();
                self.virtual_batch_commit(
                    &account_id,
                    &strategy,
                    &[(req.internal_key.clone(), req.price * req.size)],
                );
            }
            let placed_account = req.account_id.clone();
            let virtual_leg = if self
                .virtual_accounts
                .governed(placed_account.as_str(), &name)
            {
                Some((req.internal_key.clone(), req.price * req.size, req.side))
            } else {
                None
            };
            // RFC 568 阶段一：place 前回填 internal_key —— 声明腿与 OME 订单
            // 的事实钥匙挂钩（成交/死亡事件经它寻址回这条腿）。只对 BUY 入腿
            // 有意义（SELL 是退出，不入配对簿）；未声明的 token 是空操作。
            if req.side == Side::Buy {
                self.pair_bind_key(&name, placed_account.as_str(), &token, &req.internal_key);
            }
            // `entry_maker` doctrine: a pure-maker package's leg carries the
            // OME's never-escalate clock (`maker_timeout_ms < 0` — see
            // `escalation_delay_ms`); an unfilled quote rests to the round's
            // own end-of-life. Every other strategy keeps the configured
            // maker→taker window, byte-identically.
            let maker_timeout = if self
                .engine
                .as_ref()
                .is_some_and(|e| e.strategy_entry_maker(&name))
            {
                -1
            } else {
                self.config.entry_maker_timeout_ms
            };
            match self.place(req, maker_timeout, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default()
                        .placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_scoped_order_placed(placed_account.as_str(), &token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    // E-VA: a refused leg never rests — release the keyed
                    // virtual hold its batch took, so the pot reflects only
                    // commitments that actually exist.
                    if let Some((key, _notional, side)) = &virtual_leg
                        && *side == Side::Buy
                    {
                        self.virtual_accounts.release(
                            placed_account.as_str(),
                            &name,
                            key,
                            *_notional,
                        );
                    }
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    // E9-c: attach the live reason text verbatim to the receipt
                    // log line so per-strategy accounting and logs tell the
                    // same story.
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
                }
            }
        }
        placed
    }

    /// Issue #390 (replay nofast): one QUIET evaluation cycle — the base
    /// walk's every-tick Lua collection replaced by a re-submission of the
    /// standing candidates through the SAME kernel-side pipeline
    /// (`engine.replay_candidates` → the shared submit half). Everything the
    /// base cycle produced from the re-collected suggestions — the
    /// `signals`/`place_rejected` counters, the per-strategy accounting and
    /// rejection buckets, the entry-rejected log lines, the gate verdicts and
    /// their placement instants — is reproduced cycle for cycle without the
    /// Lua call; the counters are the schedule's, not the wall cost's
    /// (mirroring `replay_credit_cycles`). Deliberately NOT touched: the eval
    /// wake flags (a quiet cycle consumes neither the input wake nor the
    /// timing boundary — both belong to the next REAL evaluation, whose
    /// Lua collection refreshes the staged set), the shadow-evolution cadence
    /// (a jumping driver is refused while it is enabled), and the eval-call
    /// health counters (host diagnostic, exempt host fields).
    /// Issue #390 (replay nofast): one QUIET evaluation cycle — the base
    /// walk's every-tick Lua collection replaced by a re-submission of the
    /// standing candidates through the SAME kernel-side pipeline
    /// (`engine.replay_candidates` → the shared submit half). Everything the
    /// base cycle produced from the re-collected suggestions — the
    /// `signals`/`place_rejected` counters, the per-strategy accounting and
    /// rejection buckets, the entry-rejected log lines, the gate verdicts and
    /// their placement instants — is reproduced cycle for cycle without the
    /// Lua call; the counters are the schedule's, not the wall cost's
    /// (mirroring `replay_credit_cycles`). Returns `None` when the staged
    /// set is not in a stable phase (the caller falls back to a real
    /// evaluation). Deliberately NOT touched: the eval wake flags (a quiet
    /// cycle consumes neither the input wake nor the timing boundary — both
    /// belong to the next REAL evaluation, whose Lua collection refreshes the
    /// staged set), the shadow-evolution cadence (a jumping driver is refused
    /// while it is enabled), and the eval-call health counters (host
    /// diagnostic, exempt host fields).
    pub(super) fn engine_replay_cycle(&mut self, now_ms: i64) -> Option<usize> {
        let held_view = self.held_position_view();
        let staged = self
            .engine
            .as_mut()
            .map(|e| {
                // #202: the same per-cycle equity push the real cycle makes,
                // so a replayed ticket is sized against the balance the
                // process has NOW. The per-scope mirrors ride along (the
                // replay book is single-default — the loop is one iteration,
                // byte-identical outcomes with the pre-scope shape).
                e.set_equity_usd(self.accounts.active_ledger().balance());
                for a in self.accounts.account_list() {
                    let id = a.id.as_str();
                    let equity = self
                        .accounts
                        .ledger_of(&a.id)
                        .map(|l| l.balance())
                        .unwrap_or_default();
                    e.set_account_equity(id, equity);
                    e.set_account_trading(id, a.trading_enabled);
                    e.set_account_venue(id, &a.venue);
                    e.set_account_frozen(id, self.account_freezes.contains_key(id));
                }
                e.set_positions_view(held_view);
                e.replay_candidates(now_ms)
            })
            .unwrap_or_else(|| Some(Vec::new()))?;
        let orders = staged;
        self.stats.signals += orders.len() as u64;
        let mut tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        // ── E-VA: the SAME whole-cycle pre-check the real cycle runs (issue
        // #390's parity rule: the nofast twin reproduces the base cycle's
        // kernel-side pipeline cycle for cycle). Ungoverned scopes
        // short-circuit before touching anything.
        self.arb_precheck_filter(&mut tokens);
        // The physics projection the base cycle judged (E25): refreshed for
        // THIS cycle's round clock by `replay_candidates`.
        let policy_time_left_sec = self
            .engine
            .as_ref()
            .map(|e| e.last_time_left_sec())
            .unwrap_or(0);
        let mut placed = 0;
        for (token, mut req) in tokens {
            let name = req.strategy.clone();
            // ── #363: the policy verdict for THIS intent. A Skip refuses here
            // — the intent never reaches the RiskGate. A Place caps the
            // ticket size. The shipped defaults never bite (see the comment
            // at the real cycle's verdict map).
            let policy = self.execution_policy.clone();
            let policy_verdict = policy.map(|policy| {
                // open_positions counts LIVE positions only (expired rounds
                // are settlement-dead; see engine_evaluate's verdict map).
                let open = self.positions.open_positions();
                let open_for = |acct: &str| {
                    open.iter()
                        .filter(|p| p.account_id.as_str() == acct && p.expires_at_ms > now_ms)
                        .count() as u32
                };
                let facts = crate::execution_policy::PolicyInput {
                    available_balance: self
                        .accounts
                        .ledger_of(&req.account_id)
                        .map(|l| l.balance())
                        .unwrap_or_default(),
                    total_equity: self
                        .accounts
                        .ledger_of(&req.account_id)
                        .map(|l| l.balance())
                        .unwrap_or_default(),
                    open_positions: open_for(req.account_id.as_str()),
                    current_price: req.price,
                    time_left_sec: policy_time_left_sec,
                    symbol: req.asset.clone(),
                    recent_pnl_1h: self
                        .positions
                        .recent_pnl_1h_for(req.account_id.as_str(), now_ms),
                    consecutive_losses: self.breaker.consecutive_losses(&req.strategy),
                };
                crate::execution_policy::evaluate(&policy, req.account_id.as_str(), &facts)
            });
            match policy_verdict {
                Some(Ok(crate::execution_policy::PolicyOutput::Skip { reason })) => {
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause=policy.skip reason={reason}");
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Cooldown { seconds })) => {
                    let until = now_ms + seconds * 1000;
                    self.policy_cooldowns
                        .insert(req.account_id.to_string(), until);
                    tracing::info!(target: "strategy",
                        "policy cooldown: account={} {}s (entries resume at {})",
                        req.account_id, seconds, until);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                Some(Ok(crate::execution_policy::PolicyOutput::Place { budget_usd, .. })) => {
                    let shares = (budget_usd / req.price).floor();
                    if shares >= Decimal::ONE && req.size > shares {
                        tracing::info!(target: "strategy",
                            "policy budget shrink: strategy={} account={} {} -> {shares} shares \
                             (budget {budget_usd} USD)",
                            name, req.account_id, req.size);
                        req.size = shares;
                    }
                    if shares < Decimal::ONE {
                        tracing::info!(target: "strategy",
                            "entry rejected: strategy={name} cause=policy.skip reason=\
                             budget {budget_usd} USD cannot buy one share at {}",
                            req.price);
                        self.stats.place_rejected += 1;
                        let acc = self
                            .strategy_accounting
                            .entry(strategy_acc_key(&name, req.account_id.as_str()))
                            .or_default();
                        acc.rejected += 1;
                        *acc.rejection_causes
                            .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                            .or_default() += 1;
                        continue;
                    }
                }
                Some(Err(refusal)) => {
                    tracing::warn!(target: "strategy",
                        "entry refused: strategy={name} account={} policy refusal: {}",
                        req.account_id, refusal.reason);
                    self.stats.place_rejected += 1;
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, req.account_id.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes
                        .entry(crate::execution_policy::PolicyOutput::SKIP_BUCKET.into())
                        .or_default() += 1;
                    continue;
                }
                None => {}
            }
            // ── E25 (#331): the arbitration seam, identical to the real
            // cycle's (the gates reuse the existing implementations, so no
            // threshold has two truths).
            let account_id = req.account_id.clone();
            let is_close = crate::risk::is_close_intent(&req.internal_key);
            let account_entry_block = self
                .accounts
                .permits_order(&account_id, is_close)
                .err()
                .map(|e| e.message);
            let intent = crate::arbitration::StrategyIntent::from_request(req);
            let outcome = {
                let round_tokens = self
                    .engine
                    .as_ref()
                    .map(|e| e.round_token_ids())
                    .unwrap_or_default();
                // #461: the systemic aggregates are read BEFORE the account
                // borrow — resting BUY notional joins the exposure.
                let systemic_facts = self.systemic_facts(&intent.request.asset, now_ms);
                // Same PHYSICS doctrine read as the real cycle: a
                // hold-to-settlement declaration widens the bound stop.
                let strategy_holds_to_settlement = self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.strategy_holds_to_settlement(&intent.request.strategy));
                // E28 (§9.3): the reserve happens in THIS account's book.
                match self.accounts.get_mut(&account_id) {
                    Err(e) => crate::arbitration::Outcome {
                        decision: crate::arbitration::Decision::Rejected {
                            reason: crate::arbitration::RejectReason::AccountLimit,
                            gate: crate::arbitration::GateId::Risk,
                            detail: e.message,
                        },
                        gates: Vec::new(),
                        latency_us: 0,
                    },
                    Ok(ledger) => {
                        let equity = ledger.balance();
                        let mut ctx = crate::arbitration::IntentCtx {
                            round_tokens: &round_tokens,
                            risk: &self.risk,
                            breaker: &self.breaker,
                            ledger,
                            exit_cfg: &self.config.positions.exit,
                            time_left_sec: policy_time_left_sec,
                            now_ms,
                            equity,
                            account_entry_block,
                            systemic_facts,
                            strategy_holds_to_settlement,
                        };
                        crate::arbitration::process_intent(&intent, &mut ctx)
                    }
                }
            };
            self.record_intent_decision(&intent, &outcome, now_ms);
            if let crate::arbitration::Decision::Rejected {
                reason,
                gate,
                detail,
                ..
            } = &outcome.decision
            {
                self.stats.place_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, intent.request.account_id.as_str()))
                    .or_default();
                acc.rejected += 1;
                let cause = classify_rejection(&crate::model::CoreError::new(
                    crate::arbitration::reason_code(*reason),
                    detail.clone(),
                ));
                *acc.rejection_causes.entry(cause).or_default() += 1;
                tracing::info!(target: "strategy",
                    "entry rejected: strategy={name} cause=arbitration gate={gate:?} reason={reason:?} detail={detail}");
                continue;
            }
            let mut req = intent.request;
            if let crate::arbitration::Decision::Modified {
                modification: crate::arbitration::Modification::SizeReduced { approved, .. },
                ..
            } = &outcome.decision
            {
                tracing::info!(target: "strategy",
                    "entry modified: strategy={name} systemic shrinks {} -> {approved} shares",
                    req.size);
                req.size = *approved;
            }
            // E16/#98 portfolio-level exposure cap (0 = off).
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit, now_ms)
            {
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self
                    .strategy_accounting
                    .entry(strategy_acc_key(&name, req.account_id.as_str()))
                    .or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            // E-VA: the batch pre-check passed — HOLD this leg's virtual
            // quota before it spends anything real. A later per-order gate
            // may still refuse the leg: the Err arm below releases its keyed
            // reservation, and a placed leg keeps its hold until the order
            // state machine's terminal transition releases it.
            {
                let account_id = req.account_id.to_string();
                let strategy = req.strategy.clone();
                self.virtual_batch_commit(
                    &account_id,
                    &strategy,
                    &[(req.internal_key.clone(), req.price * req.size)],
                );
            }
            let placed_account = req.account_id.clone();
            let virtual_leg = if self
                .virtual_accounts
                .governed(placed_account.as_str(), &name)
            {
                Some((req.internal_key.clone(), req.price * req.size, req.side))
            } else {
                None
            };
            // RFC 568 阶段一：place 前回填 internal_key —— 声明腿与 OME 订单
            // 的事实钥匙挂钩（成交/死亡事件经它寻址回这条腿）。只对 BUY 入腿
            // 有意义（SELL 是退出，不入配对簿）；未声明的 token 是空操作。
            if req.side == Side::Buy {
                self.pair_bind_key(&name, placed_account.as_str(), &token, &req.internal_key);
            }
            // `entry_maker` doctrine: a pure-maker package's leg carries the
            // OME's never-escalate clock (`maker_timeout_ms < 0` — see
            // `escalation_delay_ms`); an unfilled quote rests to the round's
            // own end-of-life. Every other strategy keeps the configured
            // maker→taker window, byte-identically.
            let maker_timeout = if self
                .engine
                .as_ref()
                .is_some_and(|e| e.strategy_entry_maker(&name))
            {
                -1
            } else {
                self.config.entry_maker_timeout_ms
            };
            match self.place(req, maker_timeout, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default()
                        .placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_scoped_order_placed(placed_account.as_str(), &token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    // E-VA: a refused leg never rests — release the keyed
                    // virtual hold its batch took, so the pot reflects only
                    // commitments that actually exist.
                    if let Some((key, _notional, side)) = &virtual_leg
                        && *side == Side::Buy
                    {
                        self.virtual_accounts.release(
                            placed_account.as_str(),
                            &name,
                            key,
                            *_notional,
                        );
                    }
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self
                        .strategy_accounting
                        .entry(strategy_acc_key(&name, placed_account.as_str()))
                        .or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
                }
            }
        }
        Some(placed)
    }

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
    pub(super) fn systemic_facts(
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
    pub(super) fn counterpart_pair_basis(
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
    pub(super) fn virtual_release_order(&mut self, order: &crate::model::TrackedOrder) {
        if order.side != Side::Buy {
            return;
        }
        self.virtual_accounts.release_all(
            order.account_id.as_str(),
            &order.strategy,
            &order.internal_key,
        );
    }

    /// Per-strategy accounting (P-1.1): open exposure (live) plus session order
    /// counters and realized PnL per registered strategy.
    ///
    /// PR H: one row per strategy NAME (a strategy assigned to two accounts is
    /// one strategy — the old per-scope fan-out produced duplicate rows the
    /// panel read as separate strategies). The row aggregates across scopes and
    /// carries `accounts`: the account ids that hold a live scope of this
    /// strategy, so the panel can slice to the active account. The
    /// `enabled` flag reads "any scope enabled" — a name is either globally
    /// togglable or per-scope from the assignment write, and both arrive here.
    ///
    /// 口径（issue 552 审计）：`enabled` 是 master-power 位（默认 scope 的
    /// 全局开关），**不是** (账户,策略) 复合键的真值 —— 复合真值在
    /// `accounts`（该策略有 live scope 的账户 = 启用集点名了它）。面板已
    /// 不直接消费该位（策略页按钮按账户 feature-detect）；Overview 的
    /// 「尚无启用策略」提示走同一复合口径。
    pub fn strategy_stats(&self) -> Vec<serde_json::Value> {
        let enabled = self.enabled_strategy_names();
        // The row set is the engine's registered names UNION every name that
        // already has accounting slices. A strategy whose scope was torn down
        // (or a test core without an engine) still owns its closed-trade
        // history — dropping the row would make realized PnL vanish from the
        // page mid-session.
        let mut names = self.strategy_names();
        for k in self.strategy_accounting.keys() {
            if let Some(name) = k.split('\u{1}').next()
                && !names.iter().any(|n| n == name)
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                // Fold the (strategy, account) map across every scope of this
                // strategy: the row's counters are the strategy's TOTALS, and
                // `accounts` names the slices a per-account view can pick.
                // The set is the LIVE SCOPES (engine registry, the same
                // read `strategy.list` rows carry) UNION the accounting keys
                // (a torn-down scope's closed-trade history must keep its
                // attribution even after its scope is gone).
                let mut acc = StrategyAccounting::default();
                let prefix = format!("{name}\u{1}");
                let mut accounts: Vec<String> = self
                    .engine
                    .as_ref()
                    .map(|e| e.strategy_scopes(&name))
                    .unwrap_or_default();
                for k in self.strategy_accounting.keys() {
                    if let Some(a) = k.strip_prefix(&prefix)
                        && !accounts.iter().any(|v| v == a)
                    {
                        accounts.push(a.to_string());
                    }
                }
                accounts.sort();
                accounts.dedup();
                for a in &accounts {
                    if let Some(one) = self.strategy_accounting.get(&strategy_acc_key(&name, a)) {
                        acc.placed += one.placed;
                        acc.rejected += one.rejected;
                        acc.limit_rejected += one.limit_rejected;
                        acc.closed_trades += one.closed_trades;
                        acc.wins += one.wins;
                        acc.losses += one.losses;
                        acc.fees_usd += one.fees_usd;
                        acc.net_pnl_usd += one.net_pnl_usd;
                        for (cause, n) in &one.rejection_causes {
                            *acc.rejection_causes.entry(cause.clone()).or_default() += n;
                        }
                    }
                }
                let opens: Vec<&crate::position::OpenPosition> = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.strategy == name)
                    .collect();
                let open_notional: Decimal = opens.iter().map(|p| p.cost_usd).sum();
                // Source: the live scope's provenance when one exists, else the
                // sticky package_sources entry (a torn-down scope's row keeps
                // naming where the package lives — the strategies page shows
                // every INSTALLED strategy, enabled or not).
                let source = self
                    .engine
                    .as_ref()
                    .map(|e| {
                        e.strategy_source(&name).map(|s| s.to_string()).or_else(|| {
                            e.scope_sources()
                                .into_iter()
                                .find(|(n, _)| n == &name)
                                .map(|(_, s)| s)
                        })
                    })
                    .unwrap_or_default()
                    .unwrap_or_default();
                // E2-a: the sizing actually in force for this strategy (its own
                // override clamped by the globals, else the globals) plus the
                // configured caps, so the report shows both quota and occupancy.
                let effective = self
                    .engine
                    .as_ref()
                    .map(|e| e.effective_sizing(&name))
                    .unwrap_or(crate::engine::EffectiveSizing {
                        size_usd: self.config.size_usd,
                        min_shares: self.config.min_shares,
                        max_shares: self.config.max_shares,
                        size_pct: self.config.size_pct,
                        strategy_scoped: false,
                        size_weight: None,
                    });
                let limit = self.config.strategy_limits.get(&name);
                // E2-b: what this strategy DECLARED it does not need, next to how
                // often a gate actually stopped it and how often an exemption let
                // a candidate through — so an opt-out is never invisible beside an
                // un-exempted strategy's rejections.
                let declared = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.strategy_gate_exemptions(&name))
                    .unwrap_or_default();
                let tally = self
                    .engine
                    .as_ref()
                    .map(|e| e.gate_tally(&name))
                    .unwrap_or_default();
                let mut row = serde_json::json!({
                    "name": name,
                    "enabled": enabled.contains(&name),
                    "source": source,
                    // PR H: the account scopes holding this strategy (sorted;
                    // empty = registered but assigned to no account).
                    "accounts": accounts,
                    "openPositions": opens.len(),
                    "openNotionalUsd": dec_json(open_notional),
                    // ── E2-a quota + effective sizing ──
                    "maxOpenPositions": limit.and_then(|l| l.max_open_positions),
                    "maxOpenNotionalUsd": limit
                        .and_then(|l| l.max_open_notional_usd)
                        .map(dec_json),
                    "sizingSource": if effective.strategy_scoped { "strategy" } else { "global" },
                    "effectiveSizeUsd": dec_json(effective.size_usd),
                    "effectiveMinShares": dec_json(effective.min_shares),
                    "effectiveMaxShares": dec_json(effective.max_shares),
                    "sizeWeight": effective.size_weight.map(dec_json),
                    // #202: the equity-relative budget in force for this leg and
                    // what it is worth on the CURRENT balance, so "how much will
                    // this account commit per entry?" is a number on the panel
                    // rather than a reading of the flags.
                    "effectiveSizePct": dec_json(effective.size_pct),
                    "entryBudgetUsd": dec_json(
                        self.engine
                            .as_ref()
                            .map(|e| e.entry_budget_usd(&name))
                            .unwrap_or(effective.size_usd),
                    ),
                    // ── E2-b declared gate exemptions + per-strategy gate counts ──
                    "gateExemptions": declared.gates(),
                    // D-31: the declared `time_left_sec` floor, or null when the
                    // strategy left it to the kernel. Null is meaningful here —
                    // it says "this opt-out still stops at the global window".
                    "gateExemptionTimingFloorSec": declared.timing_min_time_left_sec,
                    "blockedTiming": tally.blocked_timing,
                    "blockedMomentum": tally.blocked_momentum,
                    "gateExemptedTiming": tally.exempted_timing,
                    "gateExemptedMomentum": tally.exempted_momentum,
                    "ordersPlaced": acc.placed,
                    "ordersRejected": acc.rejected,
                    "limitRejected": acc.limit_rejected,
                    // E9-c: cause → count; only counted buckets appear.
                    "rejectionCauses": if acc.rejection_causes.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(acc.rejection_causes)
                    },
                    "closedTrades": acc.closed_trades,
                    "wins": acc.wins,
                    "losses": acc.losses,
                    "feesUsd": dec_json(acc.fees_usd),
                    "netPnlUsd": dec_json(acc.net_pnl_usd),
                });
                // 套利合计字段（契约）：仅「声明被受理」的行携带 —— RFC-645
                // A-1（决议②③）：套利面的裁判从名字比较改为声明驱动。受理
                // 谓词 = 该策略声明 consumes_pair_book_stats() 且声明
                // atomic_batch() 且配对簿里有它 owner_strategy 名下至少一条
                // 在册（非 Closed）条目，三者缺一 = 视为未声明（不附加
                // arb* 字段），并在声明在场却未被受理时 warn 一条（说明缺
                // 哪条腿）。M2 过渡期：名字恰好等于 ARB_STRATEGY 的行仍按
                // 旧名字分支受理（保留一个发布周期），命中时打一次性的
                // deprecation 告警；M3 删名字分支。双腿平仓各记在该策略的
                // (strategy, 各腿账户) 键下（`on_position_closed`），上面的
                // fold 已经是该策略的全口径合计，原样再报一遍专名字段，前端
                // 不必知道键形状；`arbActivePairs` 读配对簿的在册（非
                // Closed）配对数。未受理行不带这些键。
                let arb_accepted = self
                    .engine
                    .as_ref()
                    .map(|e| e.pair_book_stats_accepted(&name, &self.pair_book))
                    .unwrap_or(false);
                // M2 deprecation bridge: the NAME branch (an exact
                // ARB_STRATEGY match) stays authoritative for one release
                // cycle. A row that rides the name instead of an accepted
                // declaration gets a one-shot (per process) deprecation warn;
                // a row whose declaration IS accepted rides the new path
                // silently.
                let name_bridge = name == ARB_STRATEGY && !arb_accepted;
                if name_bridge {
                    arb_name_branch_warn_once(&name);
                }
                if arb_accepted || name_bridge {
                    if let Some(reason) = self
                        .engine
                        .as_ref()
                        .and_then(|e| e.pair_book_stats_rejection_reason(&name, &self.pair_book))
                    {
                        tracing::warn!(
                            strategy = name.as_str(),
                            reason,
                            "consumes_pair_book_stats declared but NOT accepted — the arb* face \
                             rides the deprecated name branch (ARB_STRATEGY match) or is omitted \
                             entirely; a pair consumer must also declare atomic_batch and hold at \
                             least one live PairBook entry"
                        );
                    }
                    row["arbNetPnlUsd"] = dec_json(acc.net_pnl_usd);
                    row["arbWins"] = serde_json::json!(acc.wins);
                    row["arbLosses"] = serde_json::json!(acc.losses);
                    row["arbClosedTrades"] = serde_json::json!(acc.closed_trades);
                    row["arbActivePairs"] =
                        serde_json::json!(self.pair_book.active_pairs_count(&name));
                }
                row
            })
            .collect()
    }

    /// PR H: per-STRATEGY accounting for ONE account — the same counters as
    /// `strategy_stats`, folded over a single account's scopes only. The
    /// strategy page slices with this so a strategy that trades on two
    /// accounts does not print account B's PnL on account A's page.
    pub fn strategy_stats_for_account(&self, account: &str) -> Vec<serde_json::Value> {
        let mut rows = self.strategy_stats();
        // Per-scope counters of just this account, folded back over the row's
        // name — the shape stays identical to `strategy_stats` (the panel's
        // table code renders one shape), only the counter values differ.
        for row in &mut rows {
            let Some(name) = row.get("name").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            // The arb totals the GLOBAL row just carried (the arb unit's whole
            // fold across its leg accounts); read before any slice overwrites
            // the row, and re-attached below so every account's page shows the
            // arb unit's combined take — the legs' own slices stay per-account
            // in the plain counters. RFC-645 A-1: which rows carry the arb*
            // keys was decided by the acceptance predicate in `strategy_stats`
            // (declaration accepted, or the M2 name bridge) — the global row's
            // `arbNetPnlUsd` presence IS the verdict, so this read follows it
            // verbatim and the slice re-attachment cannot drift from the row
            // that produced it.
            let arb_totals = if row.get("arbNetPnlUsd").is_some() {
                Some((
                    row.get("arbNetPnlUsd").cloned().unwrap_or_default(),
                    row.get("arbWins").cloned().unwrap_or_default(),
                    row.get("arbLosses").cloned().unwrap_or_default(),
                    row.get("arbClosedTrades").cloned().unwrap_or_default(),
                    row.get("arbActivePairs").cloned().unwrap_or_default(),
                ))
            } else {
                None
            };
            let one = self
                .strategy_accounting
                .get(&strategy_acc_key(&name, account))
                .cloned()
                .unwrap_or_default();
            row["ordersPlaced"] = serde_json::json!(one.placed);
            row["ordersRejected"] = serde_json::json!(one.rejected);
            row["limitRejected"] = serde_json::json!(one.limit_rejected);
            row["rejectionCauses"] = if one.rejection_causes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(one.rejection_causes)
            };
            row["closedTrades"] = serde_json::json!(one.closed_trades);
            row["wins"] = serde_json::json!(one.wins);
            row["losses"] = serde_json::json!(one.losses);
            row["feesUsd"] = dec_json(one.fees_usd);
            row["netPnlUsd"] = dec_json(one.net_pnl_usd);
            // Gate counts follow the account too: the engine folds them per
            // (strategy, account), so this account's page never prints a
            // sibling account's blocked/exempted numbers.
            let tally = self
                .engine
                .as_ref()
                .map(|e| e.gate_tally_for_account(&name, account))
                .unwrap_or_default();
            row["blockedTiming"] = serde_json::json!(tally.blocked_timing);
            row["blockedMomentum"] = serde_json::json!(tally.blocked_momentum);
            row["gateExemptedTiming"] = serde_json::json!(tally.exempted_timing);
            row["gateExemptedMomentum"] = serde_json::json!(tally.exempted_momentum);
            // Open exposure: THIS account's slice of the strategy.
            let opens: Vec<&crate::position::OpenPosition> = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.strategy == name && p.account_id.as_str() == account)
                .collect();
            row["openPositions"] = serde_json::json!(opens.len());
            row["openNotionalUsd"] = dec_json(opens.iter().map(|p| p.cost_usd).sum());
            // The arb unit's combined take rides EVERY account's page
            // (契约：无论面板选哪个账户，套利行显示套利单元合计收益).
            if let Some((net, wins, losses, closed, pairs)) = arb_totals {
                row["arbNetPnlUsd"] = net;
                row["arbWins"] = wins;
                row["arbLosses"] = losses;
                row["arbClosedTrades"] = closed;
                row["arbActivePairs"] = pairs;
            }
        }
        rows
    }

    /// Diagnostic snapshot: feed counters + engine trend/confirmed state.
    pub fn engine_stats(&self) -> serde_json::Value {
        self.engine_stats_at(now_ms())
    }

    /// E29 (§12.2): `kline.history` body — the aggregator's closed tail plus
    /// the still-growing bar as the last element. A core without an engine
    /// answers the empty list: the documented envelope for "no bars exist",
    /// exactly what the Wave-0 placeholder arm used to answer.
    pub fn kline_history(
        &self,
        symbol: &str,
        interval: crate::kline::KlineInterval,
        limit: usize,
    ) -> Vec<crate::kline::Kline> {
        self.engine
            .as_ref()
            .map(|e| e.kline_history(symbol, interval, limit))
            .unwrap_or_default()
    }

    /// E29: aggregation counters for the stats surface (§10.3: drops must be
    /// visible). Zeros when no engine is attached.
    pub fn kline_stats(&self) -> Option<crate::kline::AggregatorStats> {
        self.engine.as_ref().map(|e| e.kline_stats())
    }

    /// The same snapshot as of an explicit instant. The token lists are sorted:
    /// their source is a `HashSet`, whose iteration order is randomised per
    /// process, and a report that shuffles between runs cannot be diffed.
    pub fn engine_stats_at(&self, as_of_ms: i64) -> serde_json::Value {
        // P0 #670: fd 配额使用率——EMFILE 自旋（accept 刷日志写满磁盘）的
        // 前夜观测。只在本次 stats 求值时算一次（面板轮询频率），不在数据/
        // 订单路径上。open/usagePct 在 macOS 上为 null（无 procfs），
        // critical 随之恒 false。
        let probe_fd = crate::probe::fd_usage();
        let probe_fd_json = serde_json::json!({
            "soft": probe_fd.as_ref().map(|u| u.soft),
            "hard": probe_fd.as_ref().map(|u| u.hard),
            "open": probe_fd.as_ref().and_then(|u| u.open),
            "usagePct": probe_fd.as_ref().and_then(|u| u.usage_pct()),
            "critical": probe_fd.as_ref().is_some_and(|u| u.critical()),
        });
        let mut confirmed = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_tokens().into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        confirmed.sort();
        let mut confirmed_detail = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_diagnostics(as_of_ms))
            .unwrap_or_default();
        confirmed_detail.sort_by(|a, b| a["token"].as_str().cmp(&b["token"].as_str()));
        let blocked = self
            .engine
            .as_ref()
            .map(|e| {
                // Per-strategy attribution (E2-b / #27): the totals stay global,
                // and `byStrategy` names whose candidates each block belonged to.
                let mut by_strategy = serde_json::Map::new();
                for name in self.strategy_names() {
                    let t = e.gate_tally(&name);
                    if t.blocked_timing == 0 && t.blocked_momentum == 0 {
                        continue;
                    }
                    by_strategy.insert(
                        name,
                        serde_json::json!({
                            "timing": t.blocked_timing,
                            "momentum": t.blocked_momentum,
                        }),
                    );
                }
                let declared: Vec<serde_json::Value> = e
                    .declared_gate_exemptions()
                    .into_iter()
                    .map(|(name, x)| {
                        // D-31: name the remaining-time floor beside the gates, so
                        // "which opt-outs reach into the closing window" is
                        // answerable from the stats snapshot alone.
                        let mut v = serde_json::json!({ "strategy": name, "gates": x.gates() });
                        if let Some(n) = x.timing_min_time_left_sec {
                            v["timingFloorSec"] = serde_json::json!(n);
                        }
                        v
                    })
                    .collect();
                serde_json::json!({
                    "timing": e.blocked_timing_count(),
                    "momentum": e.blocked_momentum_count(),
                    "byStrategy": by_strategy,
                    "declaredExemptions": declared,
                })
            })
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "books": self.stats.books,
            "tops": self.stats.tops,
            "spots": self.stats.spots,
            "rounds": self.stats.rounds,
            // #377 (治 #2): archive verdict events replayed into the core —
            // settlements that came from the stream instead of the dry ladder.
            "resolutions": self.stats.resolutions,
            // #388: wallet-level cash rows credited from the stream — the
            // count and exact USD total per kind, so a report reconciles the
            // credit against the ledger it built.
            "cashflowRebates": self.stats.cashflow_rebates,
            "cashflowRewards": self.stats.cashflow_rewards,
            "cashflowRebatesUsd": dec_json(self.stats.cashflow_rebates_usd),
            "cashflowRewardsUsd": dec_json(self.stats.cashflow_rewards_usd),
            "evaluations": self.stats.evaluations,
            "signals": self.stats.signals,
            "placeRejected": self.stats.place_rejected,
            "strategyLimitRejected": self.stats.strategy_limit_rejected,
            "venueRejected": self.stats.venue_rejected,
            "illegalPriceRejected": self.stats.illegal_price_rejected,
            // #180: the ONE panel-visible error slot — structured (code + when +
            // what), written by every internal refusal path, and read by the
            // panel's error banner. `code` is the same `CoreErrorCode`
            // vocabulary a rejected RPC carries in `data.coreCode`, so a client
            // that knows how to branch on one knows how to branch on the other.
            "lastError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "code": e.code,
                    "message": e.message,
                }),
                None => serde_json::Value::Null,
            },
            // Legacy spelling of the same record, kept because the shipped panel
            // (ui/webapp/webui/src/pages/Overview.vue) reads this key for its
            // "last trading error" banner. Same record, one writer, one
            // timestamp — never a second, staler copy. The message is rendered
            // exactly as this key always rendered it (`<code>: <message>`), so a
            // consumer that parsed the old string keeps working; `lastError`
            // above is the structured form to migrate to.
            "lastVenueError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "message": e.legacy_message(),
                }),
                None => serde_json::Value::Null,
            },
            // E31-b: how close the reconcile sweep is to freezing trading. The
            // counter is the missing half of the last error — the error says a
            // sweep failed, the counter says how many in a row, and the threshold
            // says when the freeze lands. Read-only: nothing here sets either
            // value, and the sweep's own success still resets the streak
            // (`Core::reconcile`). Exposed because the transition itself is only
            // reachable from a market plugin, so this is the only way an outside
            // observer (panel, gate) can see the kernel's own threshold.
            "reconcile": {
                "consecutiveSweepFailures": self.consecutive_sweep_failures,
                "freezeThreshold": SWEEP_FAILURE_FREEZE,
            },
            "selfCheck": match &self.last_self_check {
                Some(r) => serde_json::json!({
                    "ok": r.ok,
                    "tsMs": r.ts_ms,
                    "items": r.items.iter().map(|i| serde_json::json!({
                        "name": i.name, "ok": i.ok, "detail": i.detail,
                    })).collect::<Vec<_>>(),
                }),
                None => serde_json::Value::Null,
            },
            "tradingFrozen": if self.risk.is_killed() {
                serde_json::json!({
                    "active": true,
                    "reason": self.risk.kill_reason().unwrap_or("kill switch active"),
                })
            } else {
                serde_json::json!({ "active": false })
            },
            // The account-scoped kill book (account.freeze / auto-freeze on
            // the venue-rejection streak): account id → why. Empty object when
            // nothing is frozen — a client distinguishes "global freeze"
            // (tradingFrozen.active) from "these accounts only" without
            // parsing reasons.
            "accountFreezes": self.account_freezes,
            // In-kernel accounting audit (issue #189): the latest verdict, so the
            // panel shows what the kernel itself concluded about the books.
            "accountingAudit": self.accounting_audit_view(),
            // #173: the day's realized-loss budget as the panel reads it —
            // visible even when healthy, so "is the breaker armed, and against
            // what cap?" is answerable without grepping the startup log.
            // The daily budget is PER-ACCOUNT (2026-10-25 verdict): the card
            // mirrors the DISPLAY account's day (the panel follows the account
            // everywhere since #592), and carries every account's day beside it
            // so no book's freeze is invisible.
            "dailyLoss": {
                "account": self.accounts.display_id(),
                "dayIndex": self.positions.daily_state_for(self.accounts.display_id()).day_index,
                "realizedPnlUsd": self
                    .positions
                    .daily_pnl_for(self.accounts.display_id()),
                "limitUsd": self
                    .positions
                    .effective_daily_loss_limit_for_public(self.accounts.display_id()),
                "openingEquityUsd": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .opening_equity_usd,
                // P1 #235: WHICH book that base belongs to ("dry"/"live", null
                // when the day predates the field). The cap is only as
                // meaningful as the account it is a share of, and the panel
                // used to show a self-consistent pair of numbers from two
                // different books.
                "equityBasis": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .equity_basis,
                "tripped": self
                    .positions
                    .daily_loss_tripped_for(self.accounts.display_id()),
                "trippedAtMs": self
                    .positions
                    .daily_state_for(self.accounts.display_id())
                    .tripped_at_ms,
                "anyTripped": self
                    .positions
                    .daily_states()
                    .values()
                    .any(|s| s.tripped),
                "accounts": self
                    .positions
                    .daily_states()
                    .iter()
                    .map(|(id, s)| serde_json::json!({
                        "account": id.as_str(),
                        "realizedPnlUsd": s.realized_pnl_usd,
                        "tripped": s.tripped,
                        "dayIndex": s.day_index,
                    }))
                    .collect::<Vec<_>>(),
                // #177: protective stops the wick guard withheld — "should have
                // triggered" is a number on the panel, not a silent no-op.
                "suppressedStops": self.positions.suppressed_stop_count(),
            },
            // #205: how stale a book may be before the engine stops pricing off
            // it — the value that actually gates entries, read off the engine
            // that applies it (a backtest can configure its own). "How long
            // after the feed goes quiet does the bot stop trading?" is a runtime
            // question and must be answerable from the panel.
            "orderbookFreshness": self.orderbook_freshness_view(),
            // P0 #202: what ONE order can commit on THIS account. The ceiling is
            // read off the kernel's own knobs — no ticket can hold more than
            // `max_shares` (every leg is clamped to it) or pay over `max_price`
            // a share, and when the equity-relative cap is armed the
            // `balance × k` bound applies on top. `scripts/risk-sizing-check.mjs`
            // asserts exactly the last line of this block on a live dry core.
            "sizing": self.sizing_view(),
            // #190: the pending-close exit-reason table — how many reasons are
            // held against the bound, and what the two clean-up paths removed.
            // `orphaned` must read 0: an entry whose position is gone is the
            // ghost reason the table used to accumulate.
            "exitReasons": self.exit_reasons_view(),
            // Settlement & redemption (issue #175): settled-but-unredeemed claims
            // are money the chain still owes, and this is where the panel sees
            // them — the same outlet as everything else, no new event type.
            "settlement": self.settlement_view(as_of_ms),
            "blocked": blocked,
            "confirmed": confirmed,
            "confirmedDetail": confirmed_detail,
            "strategies": self.strategy_stats(),
            "archive": self
                .event_archive
                .as_ref()
                .map(|a| serde_json::to_value(a.status()).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            // E29 (§12.2/§10.3): the K-line aggregator's counters — drops MUST
            // be visible (§10.3), and this is where an outside observer sees
            // them. `droppedOutOfOrder` counts discarded late ticks,
            // `noDataBars` counts silently skipped empty buckets. Null when no
            // engine is attached (same posture as `archive`).
            "kline": self
                .kline_stats()
                .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            // 环境差异排查探针（任务二 1-3）：时钟偏差 / 数据流断流 / spot
            // 失效的观测面。字段名即契约（tests/env_probes.rs 钉死）：
            // * ntp: 上次 SNTP 测量（offsetMs/skewRed/失败计数）
            // * tickHealth: 最近 10s 汇总（窗口 tick 数/零 tick token/最长
            //   静默/断流 warn 计数）
            // * spot: 停更监控（最近更新时刻/停更时长/warn 计数）
            // * fd: P0 #670 的 fd 配额使用率（函数开头算好的 probe_fd_json）。
            "probe": serde_json::json!({
                "ntp": {
                    "synced": self.probe_ntp.last_offset_ms.is_some(),
                    "offsetMs": self.probe_ntp.last_offset_ms,
                    "skewRed": self.probe_ntp.skew_red(),
                    "failures": self.probe_ntp.failures,
                    "lastCheckMs": self.probe_ntp.last_check_ms,
                },
                "tickHealth": {
                    // 进行中的窗口计数（实时）。汇总不立刻清零：值被
                    // capture 留档，下一个非汇总 tick 才重新计数——
                    // 汇总瞬间读 stats 必须还能看到刚结束的窗口。
                    "windowTicks": self.probe_ticks.window_ticks,
                    "zeroTickTokens": self.probe_last_zero,
                    "maxSilenceMs": self.probe_last_max_silence_ms,
                    "streamBreakWarnings": self.probe_ticks.stream_break_warnings,
                    "lastSummaryMs": self.probe_ticks.last_summary_ms,
                },
                "spot": {
                    "lastSeenMs": self.probe_spot.last_seen_ms,
                    "staleMs": self.probe_last_spot_stale_ms,
                    "staleWarnings": self.probe_spot.stale_warnings,
                },
                "fd": probe_fd_json,
            }),
        })
    }

    /// P0 #202 — "how much can any ONE order commit on this account, right
    /// now?", answered from the kernel's own knobs so the panel (and the sizing
    /// gate) never re-derive it from the source:
    ///
    /// * `shareBandUsd` = `max_shares × max_price`: the widest ticket the
    ///   engine's own clamp allows, valued at the risk gate's own price ceiling.
    ///   This is the bound that existed before #202, and on the live 4.8 USDC
    ///   account it is 10.00 USD — 208% of the book.
    /// * `sizeBudgetUsd` (#202, when `size_pct > 0`) = `equity × pct%`.
    /// * `equityCapUsd` (#202, when the relative cap is armed) = `equity × k%`,
    ///   the bound the risk gate REJECTS over.
    /// * `worstCaseOrderUsd` = the tightest of the three. Closing intents are
    ///   exempt from the equity cap by design (`closes_exposure`), and a close
    ///   is bounded by the position it reduces — never by these knobs — so the
    ///   number describes NEW exposure, which is what "max possible loss on one
    ///   order" means for a binary-market BUY (the whole notional can go to 0).
    pub(super) fn sizing_view(&self) -> serde_json::Value {
        // E28: the sizing view describes the ACTIVE account's book (the panel
        // is a per-session view of the session's account, §9.5).
        let equity = self.accounts.active_ledger().balance();
        let globals = self.engine.as_ref().map(|e| e.global_sizing()).unwrap_or(
            crate::engine::EffectiveSizing {
                size_usd: self.config.size_usd,
                min_shares: self.config.min_shares,
                max_shares: self.config.max_shares,
                size_pct: self.config.size_pct,
                strategy_scoped: false,
                size_weight: None,
            },
        );
        let risk = self.risk.config();
        let share_band = globals.max_shares.max(Decimal::ZERO) * risk.max_price.max(Decimal::ZERO);
        let able = equity > Decimal::ZERO;
        let size_budget = (globals.size_pct > Decimal::ZERO && able)
            .then(|| equity * globals.size_pct / Decimal::ONE_HUNDRED);
        let equity_cap = (risk.max_order_notional_pct > Decimal::ZERO && able)
            .then(|| equity * risk.max_order_notional_pct / Decimal::ONE_HUNDRED);
        let mut worst = share_band;
        if let Some(b) = size_budget {
            worst = worst.min(b);
        }
        if let Some(c) = equity_cap {
            worst = worst.min(c);
        }
        serde_json::json!({
            "equityUsd": dec_json(equity),
            "minShares": dec_json(globals.min_shares),
            "maxShares": dec_json(globals.max_shares),
            "maxPrice": dec_json(risk.max_price),
            "shareBandUsd": dec_json(share_band),
            "sizePct": dec_json(globals.size_pct),
            "sizeBudgetUsd": size_budget.map(dec_json),
            "maxOrderNotionalUsd": dec_json(risk.max_order_notional),
            "maxOrderNotionalPct": dec_json(risk.max_order_notional_pct),
            "equityCapUsd": equity_cap.map(dec_json),
            "worstCaseOrderUsd": dec_json(worst),
            "worstCasePctOfEquity": if able {
                dec_json(worst / equity * Decimal::ONE_HUNDRED)
            } else {
                serde_json::Value::Null
            },
            // The escape hatch, stated where the bounds are: a close/reduce is
            // never judged by the equity cap, so a bound that would trap a
            // position cannot be read off this block either (#174/#202).
            "closesExemptFromEquityCap": true,
            "sizePctSkippedSignals": self
                .engine
                .as_ref()
                .map(|e| e.size_pct_skip_count())
                .unwrap_or(0),
        })
    }

    /// The latest accounting audit as the panel reads it. `null` until the first
    /// audit runs (the audit interval has not elapsed, or it is disabled).
    pub fn accounting_audit_view(&self) -> serde_json::Value {
        let mut v = match self.audit.report.as_ref() {
            Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            None => serde_json::json!({ "ok": true, "note": "audit not run yet" }),
        };
        v["halted"] = serde_json::json!(self.audit.halted);
        v["haltReason"] = serde_json::json!(self.audit.halt_reason);
        v["runs"] = serde_json::json!(self.audit.runs);
        v["failures"] = serde_json::json!(self.audit.failures);
        v["anchored"] = serde_json::json!(self.audit.anchor.is_some());
        // Settled-but-unredeemed payout: money owed to us, part of the identity's
        // `total` but not of `balance` until the redeem transaction confirms
        // (issue #175). `mergeReceivableUsd` splits out #461's burned-pair
        // collateral waiting on its merge receipt — both are inside
        // `receivableUsd` (the identity telescopes over all receivables).
        // The view mirrors the AUDITED identity: the ACTIVE account's own
        // receivable only (a sibling account's claim is a different wallet).
        let active_account = self.accounts.active_id().as_str().to_string();
        v["receivableUsd"] = dec_json(self.settlement.receivable_usd_for(&active_account));
        v["mergeReceivableUsd"] =
            dec_json(self.settlement.merge_receivable_usd_for(&active_account));
        v["summary"] = serde_json::json!(
            self.audit
                .report
                .as_ref()
                .map(|r| r.summary())
                .unwrap_or_default()
        );
        v
    }

    /// Snapshot the current round for the UI.
    pub fn round_view(&self) -> Option<RoundView> {
        let engine = self.engine.as_ref()?;
        let now = now_ms();
        let r = engine.scanner().round_state(now);
        let can_trade = engine.scanner().can_trade(now).is_ok();
        // One price card per ASSET: a round may carry several markets for the
        // same asset (strike ladders), and the panel keys the cards by asset —
        // duplicates there are rendered as repeated cards with colliding keys.
        let mut market_prices: Vec<MarketPriceView> = Vec::new();
        for m in &r.markets {
            if market_prices.iter().any(|p| p.asset == m.asset) {
                continue;
            }
            let up = engine
                .book_snapshot(&m.up_token_id)
                .map(|b| b.mid_price)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(m.up_price);
            let down = engine
                .book_snapshot(&m.down_token_id)
                .map(|b| b.mid_price)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(m.down_price);
            market_prices.push(MarketPriceView {
                asset: m.asset.clone(),
                up,
                down,
            });
        }
        Some(RoundView {
            slot: r.slot,
            age_sec: r.age_sec,
            time_left_sec: r.time_left_sec,
            markets: r.markets.len(),
            can_trade,
            market_prices,
        })
    }

    /// Read-only depth view for the UI (`engine.books`, E8-c 盘口深度).
    ///
    /// Reads `Core.books` — the mirror that every ingest path updates (Node
    /// `books.snapshot` bridge and the Rust-native feed alike), and the same
    /// store the exit path values open positions from — so the view can never
    /// contradict what trading actually saw. Levels and metrics come from
    /// `OrderbookSnapshot::from_levels`, the same builder the strategy layer
    /// consumes. Purely observational: no locks beyond the core's own, no side
    /// effects. An absent round or an unknown token simply yields nothing.
    ///
    /// #560 的 slot 过滤 + (venue, asset) 唯一化在这里同样适用：快照装配层
    /// 只修了 `engine.round` 面，`engine.books` 是第二个逐市场平铺的数据源
    /// ——slot 边界附近相邻两轮（当前+1 与当前+2）并存于 scanner，盘口
    /// 深度卡每资产出现两行、选择器按 (venue,asset) 撞重复 key。与
    /// `engine.round` 同判据：到期 slot = 当前 slot + 1（正在交易的一轮），
    /// round_slot == 0（老语料 serde default）照旧放行；再先到先得唯一化。
    /// 判定钟由调用方传入（`round_state` / `current_slot` 同风格）：窗口边
    /// 界附近「喂入轮恰好是视图当下认定的 trading slot」是真实链路里插件
    /// 5s 轮询自带的性质，测试要精确钉住这个关系就不能赌宿主墙钟不翻格。
    ///
    /// E2（多族读面）：过滤谓词是 scanner 的共用纯函数
    /// `is_live_round_row` —— 每行按自己的 (venue, duration) 族钟判定
    /// 「到期 slot = 该族当前 slot + 1」，`engine.round` 行情行（ipc/server）
    /// 与本视图同谓词。老语料（round_slot == 0）无条件放行，单族部署下
    /// 族钟 ≡ 全局网格（偏移 0、行声明 ≡ cfg），判定与升级前的
    /// `current_slot(now) + 1` 逐位一致。
    pub fn books_view(&self, max_levels: usize, now: i64) -> Vec<AssetBooksView> {
        let Some(engine) = self.engine.as_ref() else {
            return Vec::new();
        };
        let scanner = engine.scanner();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        // RFC-661 M3: natural rows only. The dual-key table holds every
        // venue's OWN (venue, token) rows (M1), so the round-state sweep IS
        // the whole display.
        let rows: Vec<AssetBooksView> = scanner
            .round_state(now)
            .markets
            .iter()
            .filter(|m| scanner.is_live_round_row(m, now))
            .filter(|m| seen.insert((m.venue.clone(), m.asset.clone())))
            .map(|m| AssetBooksView {
                venue: m.venue.clone(),
                asset: m.asset.clone(),
                up_token_id: m.up_token_id.clone(),
                down_token_id: m.down_token_id.clone(),
                up: Self::book_side_view(&self.books, &m.up_token_id, max_levels),
                down: Self::book_side_view(&self.books, &m.down_token_id, max_levels),
            })
            .collect();
        rows
    }

    pub(super) fn book_side_view(
        books: &HashMap<TokenId, Book>,
        token_id: &str,
        max_levels: usize,
    ) -> BookSideView {
        let Some(b) = books.get(token_id) else {
            return BookSideView::empty();
        };
        if b.bids.is_empty() && b.asks.is_empty() {
            return BookSideView::empty();
        }
        let s = OrderbookSnapshot::from_levels(
            token_id.to_string(),
            b.bids.clone(),
            b.asks.clone(),
            now_ms(),
        );
        let level = |(p, q): &(Decimal, Decimal)| BookLevelView {
            price: *p,
            size: *q,
        };
        BookSideView {
            bids: s.bids.iter().take(max_levels).map(level).collect(),
            asks: s.asks.iter().take(max_levels).map(level).collect(),
            best_bid: Some(s.best_bid).filter(|p| *p > Decimal::ZERO),
            best_ask: Some(s.best_ask).filter(|p| *p < Decimal::ONE),
            mid_price: Some(s.mid_price).filter(|p| *p > Decimal::ZERO),
            obi: Some(s.obi),
            spread: Some(s.spread),
            spread_pct: Some(s.spread_pct),
        }
    }

    /// Open positions enriched for the UI (unrealised PnL uses the current price
    /// we track; the book tick updates it on every check).
    pub fn position_views(&self, now_ms: i64) -> Vec<crate::ipc::schema::PositionView> {
        use crate::exit_policy::pnl_pct;
        self.positions
            .open_positions()
            .iter()
            .map(|p| crate::ipc::schema::PositionView {
                id: p.id.clone(),
                asset: p.asset.clone(),
                direction: p.direction.as_str().to_string(),
                strategy: p.strategy.clone(),
                token_id: p.token_id.clone(),
                // E28 (§9.2): the account the position settles into — a field
                // read, and the row the panel filters per account.
                account_id: p.account_id.as_str().to_string(),
                // #427: the Venue column — a field read off the position.
                venue: p.venue.clone(),
                entry_price: p.entry_price,
                current_price: p.current_price,
                shares: p.shares,
                unrealized_pct: pnl_pct(p.current_price, p.entry_price),
                high_pnl_pct: p.high_pnl_pct(),
                was_maker_entry: p.was_maker_entry,
                entered_at_ms: p.entered_at_ms,
                expires_at_ms: p.expires_at_ms,
                remaining_sec: (p.expires_at_ms - now_ms) / 1000,
                // Slot matching for the panel's expiry judgment — same
                // grid coordinate the orders tab already carries.
                round_slot: p.round_slot,
                // The position's OWN family duration (three-state fallback
                // resolved exactly as the force-close gate resolves it), so
                // the panel's per-row slot arithmetic uses the family clock
                // the kernel judges with — not the page's identity-venue
                // duration, which is a different family under multi-family
                // deployment (issue 733).
                round_duration_sec: p
                    .effective_round_duration_sec(None, self.config.round_duration_sec),
                // Cash actually moved on this position, so an external monitor
                // can check the ledger identity without waiting for a flat book.
                // `entry_cost_usd` is the TOTAL entry notional (what left the
                // ledger); `cost_usd` is only the basis still held, so it is the
                // former the identity must sum.
                entry_cost_usd: p.flows.entry_cost_usd,
                cost_usd: p.cost_usd,
                entry_fee_usd: p.flows.entry_fee_usd,
                proceeds_usd: p.flows.proceeds_usd,
                exit_fee_usd: p.flows.exit_fee_usd,
            })
            .collect()
    }
}
