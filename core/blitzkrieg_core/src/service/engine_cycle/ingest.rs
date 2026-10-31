//! The market-data -> engine feed (#780): pure move from
//! `service/engine_cycle`.

use super::*;

impl Core {
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
}
