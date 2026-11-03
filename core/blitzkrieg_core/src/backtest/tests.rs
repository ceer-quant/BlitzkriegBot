use super::*;
use crate::data_source::open_replay;
use crate::engine::DataEvent;
use crate::exit_policy::ExitConfig;
use crate::model::Mode;
use crate::position::PositionConfig;
use crate::risk::RiskConfig;
use crate::test_fixtures::round_market;
use rust_decimal_macros::dec;
use std::path::PathBuf;

/// A round slot boundary is far away at this clock, so nothing expires.
const NOW: i64 = 1_000_000;

/// The production-shaped scenario (same as the P-1.1 dispatch tests, driven by
/// the clock instead of by hand): a confirmed UP trend on a calm book, one dip
/// that arms a resting maker entry, then a +100% book that lets the exit policy
/// take profit. On the feed-ws path the entry fills at the escalation deadline
/// (D-11), which is what this replay reproduces.
fn scenario_events(now: i64) -> Vec<TimedEvent> {
    let mut evs = vec![TimedEvent {
        at_ms: now,
        event: DataEvent::RoundMarkets {
            markets: vec![round_market(now)],
            now_ms: now,
        },
    }];
    for i in 0..12 {
        let t = now + i * 1000;
        evs.push(TimedEvent {
            at_ms: t,
            event: DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: t,
            },
        });
    }
    let t = now + 12_000;
    evs.push(TimedEvent {
        at_ms: t,
        event: DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: t,
        },
    });
    evs.push(TimedEvent {
        at_ms: t,
        event: DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: t,
        },
    });
    // The take-profit book arrives AFTER the escalation deadline: the
    // escalated taker leg fills against the 12 s book's 0.45 ask (the
    // honest FOK price), and the +100% bid then fires the exit policy.
    let t = now + 19_000;
    evs.push(TimedEvent {
        at_ms: t,
        event: DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.99), dec!(100))],
            asks: vec![(dec!(1.0), dec!(100))],
            now_ms: t,
        },
    });
    evs
}

/// A core config that can trade the scenario, built the way production builds
/// it: `CoreConfig::install_engine` derives the engine from these knobs.
fn base_core(archive: Option<String>) -> CoreConfig {
    CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        assets: vec!["BTC".into()],
        round_duration_sec: 900,
        min_round_age_sec: 0,
        trend_confirm_sec: 5,
        trend_window_floor_ms: 0,
        auto_exits_enabled: true,
        positions: PositionConfig {
            exit: ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        event_archive_path: archive,
        ..Default::default()
    }
}

/// `bt` but keeping the Result: the #354 cadence gate refuses a replay
/// whose corpus disagrees with the configured grid, and the tests below
/// need to see that refusal, not just a panic.
fn bt_result(
    core: CoreConfig,
    src: Box<dyn DataSource>,
    tail_ms: i64,
) -> Result<BacktestReport, String> {
    let mut b = EventBacktester::new(
        BacktestConfig {
            core,
            tick_ms: 50,
            tail_ms,
            hot_params: Vec::new(),
            progress: None,
            mode: String::new(),
        },
        src,
    );
    // PR-B: the kernel registers no strategies, so a replay that drives the
    // reference dip buyer hosts the test adapter and enables it, exactly as
    // the production path enables a loaded library.
    b.host_strategy(Box::new(
        crate::strategies::test_support::TestSpreadArb::new(
            crate::signal::TrendConfig {
                confirm_sec: 5,
                window_floor_ms: 0,
                ..Default::default()
            },
            Default::default(),
        ),
    ));
    assert!(
        b.core_mut().set_strategy_enabled("spread_arb", true),
        "the hosted adapter registers under its reference name"
    );
    b.run()
}

fn bt(core: CoreConfig, src: Box<dyn DataSource>, tail_ms: i64) -> BacktestReport {
    bt_result(core, src, tail_ms).expect("replay runs")
}

// ── #879 · the report names its coordinate system ──────────────────────

/// A mode run's text report OPENS with the coordinate-system line (slippage
/// overlay + fee tier), so two numbers are never compared across different
/// frictions. mine reads 「坐标系：mine(0 tick, <fee> 费)」; verify/sweep
/// print their own tier. The JSON wire is untouched (`serde(skip)`), and the
/// historic no-`--backtest-mode` path predates the annotation: unchanged.
#[test]
fn mode_reports_open_with_the_coordinate_system_line() {
    let run = |mode: &str| {
        EventBacktester::new(
            BacktestConfig {
                core: CoreConfig::default(),
                tick_ms: 50,
                tail_ms: 0,
                hot_params: Vec::new(),
                progress: None,
                mode: mode.to_string(),
            },
            Box::new(VecSource::new(Vec::new())),
        )
        .run()
        .expect("an empty replay still reports")
    };
    let mine = run("mine");
    // The fee name is read off the report itself (order-independent across
    // the process's fee-schedule global): "legacy 费" in the ruling's words
    // is `legacy_quadratic` on the shipped default.
    assert!(
        mine.render().starts_with(&format!(
            "坐标系：mine(0 tick, {} 费)",
            mine.fee_schedule.name
        )),
        "#879 acceptance line, verbatim tier:\n{}",
        mine.render()
    );
    let verify = run("verify");
    assert!(
        verify.render().starts_with("坐标系：verify("),
        "verify prints its own tier:\n{}",
        verify.render()
    );
    assert!(
        !run("").render().contains("坐标系"),
        "the historic single-run path stays byte-identical"
    );
}

// ── #354 (5.4) · the replay grid is fail-closed against the corpus ─────
//
// The converter stamps every market with the duration its slug declares.
// A 5m corpus replayed on the 15m grid would run every scanner gate on
// the wrong clock — silently. The gate refuses the replay; the
// lazily-correct answer to "corpus and config disagree" is an error, not
// a plausible-looking number.

/// A market the way the converter writes it: slot and expiry derived from
/// the round's OWN duration, and that duration declared on the market.
fn market_declaring(now: i64, duration_sec: i64) -> crate::model::CryptoMarket {
    let mut m = round_market(now);
    m.round_duration_sec = duration_sec;
    m.round_slot = now / 1000 / duration_sec;
    m.expires_at_ms = (m.round_slot + 1) * duration_sec * 1000;
    m
}

fn round_markets_event(now: i64, duration_sec: i64) -> TimedEvent {
    TimedEvent {
        at_ms: now,
        event: DataEvent::RoundMarkets {
            markets: vec![market_declaring(now, duration_sec)],
            now_ms: now,
        },
    }
}

/// RED must stay red: a 5m corpus replayed on the 15m grid is refused,
/// not mis-timed into a report that looks fine.
#[test]
fn a_five_minute_corpus_on_a_fifteen_minute_grid_is_refused() {
    let core = base_core(None); // the 900s grid
    let err = bt_result(
        core,
        Box::new(VecSource::new(vec![round_markets_event(NOW, 300)])),
        0,
    )
    .expect_err("5m corpus on the 15m grid must be refused");
    assert!(
        err.contains("300") && err.contains("900"),
        "the refusal names both cadences: {err}"
    );
}

/// The same corpus on the matching grid replays cleanly.
#[test]
fn a_five_minute_corpus_replays_on_its_own_grid() {
    let mut core = base_core(None);
    core.round_duration_sec = 300;
    let r = bt_result(
        core,
        Box::new(VecSource::new(vec![round_markets_event(NOW, 300)])),
        0,
    )
    .expect("the declared cadence matches the configured grid");
    assert!(r.errors.is_empty(), "a clean replay reports no errors");
}

/// Pre-#354 archives declare nothing (`round_duration_sec = 0`): they ride
/// the configured grid exactly as the live host does — silently, on any grid.
#[test]
fn an_undeclared_round_replays_silently_on_any_grid() {
    let core = base_core(None); // the 900s grid
    let mut m = round_market(NOW); // slot/expiry already on that grid
    m.round_duration_sec = 0; // the pre-#354 shape: no declaration at all
    let r = bt_result(
        core,
        Box::new(VecSource::new(vec![TimedEvent {
            at_ms: NOW,
            event: DataEvent::RoundMarkets {
                markets: vec![m],
                now_ms: NOW,
            },
        }])),
        0,
    )
    .expect("undeclared archives stay compatible");
    assert!(r.errors.is_empty());
}

/// B1d (轮次多族): a corpus carrying TWO declared families (the live
/// kernel's multi-family shape) cannot run on one replay grid — the
/// refusal names every foreign family as (venue, duration) and spells
/// the split instruction. RED must stay red: the old single-value
/// wording died with the single-value kernel.
#[test]
fn a_mixed_family_corpus_refuses_with_the_family_list() {
    let core = base_core(None); // the 900s grid
    let mut m1 = market_declaring(NOW, 300);
    m1.venue = "polymarket".into();
    let mut m2 = market_declaring(NOW, 3600);
    m2.venue = "kalshi".into();
    let err = bt_result(
        core.clone(),
        Box::new(VecSource::new(vec![TimedEvent {
            at_ms: NOW,
            event: DataEvent::RoundMarkets {
                markets: vec![m1.clone(), m2],
                now_ms: NOW,
            },
        }])),
        0,
    )
    .expect_err("two foreign families must be refused");
    assert!(err.contains("polymarket, 300s"), "{err}");
    assert!(err.contains("kalshi, 3600s"), "{err}");
    assert!(
        err.contains("分段跑"),
        "the split instruction rides the refusal: {err}"
    );
    // The same venue repeated at the SAME foreign duration collapses to
    // one listed family (the dedup keeps the message readable).
    let mut m3 = market_declaring(NOW, 300);
    m3.venue = "polymarket".into();
    let err2 = bt_result(
        core,
        Box::new(VecSource::new(vec![TimedEvent {
            at_ms: NOW,
            event: DataEvent::RoundMarkets {
                markets: vec![m1.clone(), m3],
                now_ms: NOW,
            },
        }])),
        0,
    )
    .expect_err("one foreign family (twice) is still refused");
    assert_eq!(err2.matches("polymarket, 300s").count(), 1, "{err2}");
}
/// #354 (5.1): the replay charges the ACTIVE schedule's curve on taker
/// legs and NOTHING on maker legs. The scenario's entry is an escalated
/// taker (10 shares at the 0.45 ask); its take-profit exit is a maker at
/// the bid. Strict equality against the charge-site's own formula pins
/// all three claims at once: the entry fee follows the real schedule
/// curve (not a flat rate), the maker exit contributes zero, and a taker
/// fee charged on a maker exit breaks the equality. RED on any of those.
#[test]
fn the_replay_charges_the_schedule_curve_on_taker_legs_and_zero_on_maker_legs() {
    let r = bt(
        base_core(None),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    assert_eq!(r.trades.closed, 1, "\n{}", r.render());
    // Entry: the escalated taker leg crosses at the 12s book's ask (0.45),
    // 10 shares (size_usd 2.5 clamps up to min_shares 10).
    let expected = (crate::exit_policy::taker_fee_pct(dec!(0.45)) / Decimal::ONE_HUNDRED)
        * dec!(0.45)
        * dec!(10);
    assert!(
        expected > Decimal::ZERO,
        "the scenario must actually incur a fee, or this pin is vacuous"
    );
    assert_eq!(
        r.trades.fees_usd,
        expected,
        "fees must be exactly the taker entry leg's schedule fee — no more \
         (a maker exit pays nothing) and no less (the entry pays the curve):\n{}",
        r.render()
    );
    // The report names the schedule the fees were charged under (#203).
    let active = crate::exit_policy::fee_schedule();
    assert_eq!(r.fee_schedule.name, active.name);
    assert_eq!(r.fee_schedule.rate, active.rate);
    assert_eq!(r.fee_schedule.exponent, active.exponent);
}

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("blitzkrieg-bt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ── #351 反向验收 · 100% reproducibility must be observable ─────────────
//
// The spec demands "100% 可复现（SHA256 校验）"; a report that merely
// LOOKS deterministic is not acceptance — the digest must pin the exact
// trade list, byte for byte.

/// ACCEPTANCE (#351): replaying the same scenario twice produces the same
/// `trades_sha256`, and any difference in the trade list shows in it.
#[test]
fn the_same_replay_twice_hashes_identically() {
    let a = bt(
        base_core(None),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    let b = bt(
        base_core(None),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    assert_eq!(
        a.trades.closed,
        1,
        "fixture sanity: the scenario must trade:\n{}",
        a.render()
    );
    assert!(!a.trades_sha256.is_empty(), "the digest must be present");
    assert_eq!(
        a.trades_sha256, b.trades_sha256,
        "identical scenario + identical knobs must hash identically"
    );

    // A different knob changes the trade list, so it must change the hash —
    // the digest is a fingerprint of the fills, not a constant.
    let slipped_core = CoreConfig {
        fill_model: crate::sim::FillModel {
            taker_slippage_ticks: 2,
            ..Default::default()
        },
        ..base_core(None)
    };
    let c = bt(
        slipped_core,
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    assert_ne!(
        a.trades_sha256, c.trades_sha256,
        "a changed trade list must change the digest"
    );
}

fn strategy<'a>(r: &'a BacktestReport, name: &str) -> &'a Value {
    r.strategies
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {}", r.render()))
}

#[test]
fn identity_replay_trades_the_scenario_and_reports_it() {
    let r = bt(
        base_core(None),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    assert_eq!(
        r.trades.closed,
        1,
        "one round trip expected:\n{}",
        r.render()
    );
    assert_eq!(
        r.trades.wins,
        1,
        "the +100% book must take profit:\n{}",
        r.render()
    );
    assert!(r.trades.net_pnl_usd > Decimal::ZERO, "\n{}", r.render());
    assert_eq!(r.open_positions, 0, "the exit must flatten the position");
    assert_eq!(
        r.orders.filled,
        2,
        "escalated entry + exit:\n{}",
        r.render()
    );
    assert_eq!(
        r.orders.cancelled, 1,
        "the unfilled maker is cancelled at escalation"
    );
    assert_eq!(r.fills, 2);
    assert_eq!(r.source_stats.events, 16);
    assert_eq!(r.fill_model, FillModel::default());
    assert_eq!(r.entry_maker_timeout_ms, 5000);
    // The report's trade list and the strategy ledger must be the same money.
    let s = strategy(&r, "spread_arb");
    assert_eq!(json_decimal(s.get("netPnlUsd")), Some(r.trades.net_pnl_usd));
    assert_eq!(json_decimal(s.get("feesUsd")), Some(r.trades.fees_usd));
    assert!(
        r.trades.fees_usd > Decimal::ZERO,
        "the escalated taker entry pays a fee"
    );
    // #183: the report also states the fill rate it actually achieved, so an
    // acceptance run cannot read as "everything filled" by assumption.
    assert_eq!(r.fill_stats.orders, 3, "entry maker, escalated leg, exit");
    assert_eq!(r.fill_stats.filled_orders, 2);
    assert_eq!(r.fill_stats.partial_orders, 0);
    assert_eq!(r.fill_stats.unfilled_orders, 1, "the cancelled maker entry");
    assert_eq!(r.fill_stats.partial_rate_pct, Decimal::ZERO);
    assert!(
        r.fill_stats.size_fill_ratio_pct > Decimal::ZERO
            && r.fill_stats.size_fill_ratio_pct < Decimal::ONE_HUNDRED,
        "the cancelled entry traded no size: {}",
        r.fill_stats.size_fill_ratio_pct
    );
}

/// #183 ACCEPTANCE: a shallow crossing book leaves the entry PARTIALLY
/// filled, and the report says so — before the depth cap this replay read as
/// a 100% fill and hid the live-bug-② shape entirely.
#[test]
fn a_shallow_book_reports_a_partial_fill() {
    let mut cfg = base_core(None);
    // No exit management: the subject here is the entry's fill, not what the
    // exit policy would do with a one-share position afterwards.
    cfg.auto_exits_enabled = false;
    let mut b = EventBacktester::new(
        BacktestConfig {
            core: cfg,
            tick_ms: 50,
            tail_ms: 0,
            hot_params: Vec::new(),
            progress: None,
            mode: String::new(),
        },
        Box::new(VecSource::new(vec![
            TimedEvent {
                at_ms: NOW,
                event: DataEvent::RoundMarkets {
                    markets: vec![round_market(NOW)],
                    now_ms: NOW,
                },
            },
            TimedEvent {
                at_ms: NOW,
                event: DataEvent::Book {
                    token_id: "up".into(),
                    bids: vec![(dec!(0.42), dec!(100))],
                    asks: vec![(dec!(0.44), dec!(100))],
                    now_ms: NOW,
                },
            },
            // The dip: one share rests at the resting bid's own price, so
            // the maker trades exactly that one share and no more.
            TimedEvent {
                at_ms: NOW + 1_000,
                event: DataEvent::Book {
                    token_id: "up".into(),
                    bids: vec![(dec!(0.42), dec!(100))],
                    asks: vec![(dec!(0.43), dec!(1))],
                    now_ms: NOW + 1_000,
                },
            },
        ])),
    );
    b.core_mut()
        .place(
            crate::model::OrderRequest {
                token_id: "up".into(),
                condition_id: "cond".into(),
                side: crate::model::Side::Buy,
                mode: crate::model::FillPolicy::Maker,
                price: dec!(0.43),
                size: dec!(5),
                internal_key: "entry:shallow".into(),
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: "up".into(),
                round_slot: NOW / 1000 / 900,
                account_id: crate::model::default_account_id(),
            },
            0,
            NOW,
        )
        .expect("the maker entry places");

    let r = b.run().expect("replay runs");
    assert_eq!(r.fill_stats.orders, 1, "\n{}", r.render());
    assert_eq!(
        r.fill_stats.partial_orders,
        1,
        "1 of 5 shares traded:\n{}",
        r.render()
    );
    assert_eq!(r.fill_stats.filled_orders, 0);
    assert_eq!(r.fill_stats.unfilled_orders, 0);
    assert_eq!(
        r.fill_stats.size_fill_ratio_pct,
        dec!(20),
        "1 filled of 5 requested"
    );
    assert_eq!(r.fill_stats.partial_rate_pct, dec!(100));
    assert_eq!(r.fills, 1, "one partial fill is still one fill event");
    // And the render must show it, not just the JSON.
    assert!(
        r.render().contains("partial fill"),
        "the human report says what traded:\n{}",
        r.render()
    );
}

/// A real feed delivers bursts of sub-millisecond events, but maintenance in
/// live runs on the server's `tick_ms` *timer*, not on arrivals. Regression:
/// with an event-gap-driven cycle a 13-minute real capture replayed 803
/// maintenance cycles against the live core's 15 618, so exits fired late and
/// the per-cycle blocked tallies were starved (D-12/§35).
#[test]
fn dense_stream_keeps_live_evaluation_cadence() {
    let mut evs = vec![TimedEvent {
        at_ms: NOW,
        event: DataEvent::RoundMarkets {
            markets: vec![round_market(NOW)],
            now_ms: NOW,
        },
    }];
    for i in 0..=2_000 {
        let t = NOW + i;
        evs.push(TimedEvent {
            at_ms: t,
            event: DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60000),
                now_ms: t,
            },
        });
    }
    // Out-of-order arrivals are a normal property of a multi-stream feed:
    // the late event must be clamped, never rewind the clock, and must not
    // buy itself an extra maintenance cycle.
    evs.push(TimedEvent {
        at_ms: NOW + 500,
        event: DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60001),
            now_ms: NOW + 500,
        },
    });

    let r = bt(base_core(None), Box::new(VecSource::new(evs)), 0);

    // 2 000 ms of stream at this fixture's 50 ms replay cadence = 40 maintenance cycles,
    // regardless of the 2 002 events that arrived in between.
    assert_eq!(r.feed["evaluations"], 40, "\n{}", r.render());
    assert_eq!(
        r.source_stats.events, 2_003,
        "every event is still delivered"
    );
    assert_eq!(
        r.virtual_ms, 2_000,
        "the late event must not move the clock"
    );
}

/// The other half of the cadence contract: a sparse stream (gaps far larger
/// than `tick_ms`) must still fire every scheduled cycle, so a quiet market
/// cannot stop exits from being evaluated.
#[test]
fn sparse_stream_evaluates_every_due_cycle() {
    let events = vec![
        TimedEvent {
            at_ms: NOW,
            event: DataEvent::RoundMarkets {
                markets: vec![round_market(NOW)],
                now_ms: NOW,
            },
        },
        TimedEvent {
            at_ms: NOW + 10_000,
            event: DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60000),
                now_ms: NOW + 10_000,
            },
        },
    ];
    let r = bt(base_core(None), Box::new(VecSource::new(events)), 0);
    assert_eq!(r.feed["evaluations"], 200, "10 000 ms / 50 ms per cycle");
}

#[test]
fn live_archive_replay_is_identical() {
    let dir = tmp_dir("equiv");
    let archive = dir.join("events.jsonl").display().to_string();
    let events = scenario_events(NOW);

    // "Live" run: the real core consumes the events and mirrors them to disk.
    let live = bt(
        base_core(Some(archive.clone())),
        Box::new(VecSource::new(events)),
        10_000,
    );
    assert!(
        std::path::Path::new(&archive).exists(),
        "the live run must record an archive"
    );
    assert!(
        live.trades.closed >= 1,
        "the equivalence fixture must trade:\n{}",
        live.render()
    );

    // Backtest run: the same core, fed from the archive.
    let replay = bt(
        base_core(None),
        Box::new(open_replay(&archive).unwrap()),
        10_000,
    );

    assert_eq!(
        replay.source_stats, live.source_stats,
        "same event stream, no parse damage"
    );
    assert_eq!(replay.source_stats.malformed_lines, 0);
    assert_eq!(replay.start_at_ms, live.start_at_ms);
    assert_eq!(replay.end_at_ms, live.end_at_ms);
    assert_eq!(replay.trades.closed, live.trades.closed);
    assert_eq!(replay.trades.wins, live.trades.wins);
    assert_eq!(replay.trades.losses, live.trades.losses);
    assert_eq!(
        replay.trades.net_pnl_usd, live.trades.net_pnl_usd,
        "PnL must match exactly"
    );
    assert_eq!(replay.trades.fees_usd, live.trades.fees_usd);
    assert_eq!(replay.fills, live.fills);
    assert_eq!(replay.orders.filled, live.orders.filled);
    assert_eq!(replay.orders.cancelled, live.orders.cancelled);
    assert_eq!(replay.open_positions, live.open_positions);
    assert_eq!(replay.feed["books"], live.feed["books"]);
    assert_eq!(replay.feed["evaluations"], live.feed["evaluations"]);
    assert_eq!(replay.blocked, live.blocked);
    assert_eq!(
        replay.strategies, live.strategies,
        "per-strategy ledger must match"
    );
    assert_eq!(
        replay
            .trade_lines
            .iter()
            .map(|t| (t.asset.clone(), t.reason.clone(), t.net_pnl_usd))
            .collect::<Vec<_>>(),
        live.trade_lines
            .iter()
            .map(|t| (t.asset.clone(), t.reason.clone(), t.net_pnl_usd))
            .collect::<Vec<_>>(),
        "trade list must match"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_archive_lines_are_skipped_without_changing_the_result() {
    let dir = tmp_dir("badline");
    let archive = dir.join("events.jsonl").display().to_string();
    let clean = bt(
        base_core(Some(archive.clone())),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );

    // Damage the tail the way a killed process would: a truncated record plus
    // junk that no writer should ever produce.
    let text = std::fs::read_to_string(&archive).unwrap();
    let last = text.lines().last().unwrap();
    let mut damaged = text.clone();
    damaged.push_str(&last[..last.len() / 2]);
    damaged.push_str("\nnot json at all\n");
    std::fs::write(&archive, damaged).unwrap();

    let replay = bt(
        base_core(None),
        Box::new(open_replay(&archive).unwrap()),
        10_000,
    );
    assert_eq!(
        replay.source_stats.malformed_lines, 2,
        "truncated + junk lines are both counted"
    );
    assert_eq!(
        replay.source_stats.events, clean.source_stats.events,
        "no event is lost"
    );
    assert_eq!(
        replay.trades.closed, clean.trades.closed,
        "a damaged tail is not fatal"
    );
    assert_eq!(replay.trades.wins, clean.trades.wins);
    assert_eq!(replay.trades.net_pnl_usd, clean.trades.net_pnl_usd);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slippage_worsens_the_replay_pnl() {
    let events = scenario_events(NOW);
    let identity = bt(
        base_core(None),
        Box::new(VecSource::new(events.clone())),
        10_000,
    );

    let mut slipped_core = base_core(None);
    slipped_core.fill_model = FillModel {
        taker_slippage_ticks: 2,
        ..FillModel::default()
    };
    let slipped = bt(slipped_core, Box::new(VecSource::new(events)), 10_000);

    assert_eq!(slipped.trades.closed, identity.trades.closed);
    let diff = identity.trades.net_pnl_usd - slipped.trades.net_pnl_usd;
    // Only TAKER legs slip: here the escalated entry buys 2 ticks higher
    // (0.02 x 10 shares = 0.20), while the take-profit exit rests at its own
    // limit and must not slip — a maker fill is priced by your own quote.
    assert!(
        diff > dec!(0.19) && diff < dec!(0.25),
        "expected ~0.20 of taker slippage cost, got {diff}\n{}",
        slipped.render()
    );
}

#[test]
fn a_long_escalation_deadline_leaves_the_maker_entry_unfilled() {
    // Documents the current live fill path (D-11): on a feed-driven replay the
    // resting maker entry fills at the escalation deadline, so pushing that
    // deadline past the replay window means no entry at all.
    let mut core = base_core(None);
    core.entry_maker_timeout_ms = 60_000;
    let r = bt(core, Box::new(VecSource::new(scenario_events(NOW))), 10_000);
    assert_eq!(
        r.orders.orders,
        1,
        "the entry is still placed:\n{}",
        r.render()
    );
    assert_eq!(
        r.orders.live_at_end, 1,
        "it is still resting when the replay ends"
    );
    assert_eq!(r.fills, 0);
    assert_eq!(r.trades.closed, 0);
    assert_eq!(r.open_positions, 0);

    // Same events, default deadline: the escalation fills it and the exit closes.
    let r2 = bt(
        base_core(None),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    assert_eq!(r2.fills, 2);
    assert_eq!(r2.trades.closed, 1);
}

/// The `-1` off switch at the replay layer — a pure-maker evaluation run.
///
/// The window here is LONGER than the 5000 ms default, which is what makes
/// this an assertion rather than a restatement: under the old "0 or negative
/// falls back to the default" semantics this exact replay armed a 5000 ms
/// clock, escalated, filled twice and closed a trade. Disarmed, the maker
/// rests to the end and nothing else happens.
#[test]
fn a_negative_entry_timeout_never_escalates_in_a_replay() {
    let mut core = base_core(None);
    core.entry_maker_timeout_ms = -1;
    let r = bt(core, Box::new(VecSource::new(scenario_events(NOW))), 10_000);
    assert_eq!(
        r.orders.orders,
        1,
        "the entry is still placed:\n{}",
        r.render()
    );
    assert_eq!(r.orders.live_at_end, 1, "it rests to the end of the replay");
    assert_eq!(r.fills, 0, "no taker leg ever fires");
    assert_eq!(r.trades.closed, 0);
    assert_eq!(r.open_positions, 0);
}

#[test]
fn the_verdict_basis_keeps_only_zero_and_the_top_rung() {
    let nets = vec![dec!(10), dec!(-5), dec!(1)];
    assert_eq!(verdict_basis_nets(&nets), vec![dec!(10), dec!(1)]);
    assert_eq!(
        latency_verdict(&verdict_basis_nets(&nets)),
        "首尾档均为正 → 延迟不敏感（稳）"
    );
}

// ── #862 · 每笔成交带 MarketRegime 标签（单一口径：strategy_logic 参考实现）──

/// Every closed trade carries the token's MarketRegime verdict, computed by
/// the strategy_logic reference classifier fed the token's own mid path —
/// never a second regime definition. The scenario's book is calm around the
/// entry, so the traded token's label comes out of the known four-state set
/// (a flat path reads `range`), and a token the replay never saw a book for
/// would read `none` — a coverage fact, never a made-up regime.
#[test]
fn trade_lines_carry_the_canonical_regime_label() {
    let r = bt(
        base_core(None),
        Box::new(VecSource::new(scenario_events(NOW))),
        10_000,
    );
    assert!(
        !r.trade_lines.is_empty(),
        "fixture sanity: the scenario trades:\n{}",
        r.render()
    );
    const KNOWN: [&str; 4] = ["range", "trendUp", "trendDown", "volatile"];
    for line in &r.trade_lines {
        assert!(
            KNOWN.contains(&line.regime.as_str()),
            "trade {} must carry a canonical regime label, got {:?}",
            line.id,
            line.regime
        );
        assert_ne!(
            line.regime, "none",
            "the scenario fed books for the traded token; `none` would mean the sampler missed them"
        );
    }
}

/// The l2-delay preset runs end-to-end through the replay harness and is
/// visible in the report's model line — the coordinate a reconciliation
/// reads before trusting any fill-price comparison.
#[test]
fn l2_delay_preset_replays_and_names_itself_in_the_report() {
    let core = CoreConfig {
        fill_model: crate::sim::fill_model_preset("l2-delay").unwrap(),
        ..base_core(None)
    };
    let r = bt(core, Box::new(VecSource::new(scenario_events(NOW))), 10_000);
    let rendered = r.render();
    let line = rendered
        .lines()
        .find(|l| l.contains("fill model"))
        .expect("the report names its fill model");
    assert!(
        line.contains("maker rule trade-through") && line.contains("taker latency 150+150 ms"),
        "the l2-delay coordinate must be readable from the report: {line}"
    );
}

// ── #916 · --backtest-knob dispatches on the strategy's OWN declaration ────

/// A strategy that declares ONE string tunable and writes every override it
/// heard somewhere host-visible. The point is the CHANNEL: a string tunable
/// must never reach the Decimal registry and a word must never parse as one.
struct StringKnobStrategy {
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl StringKnobStrategy {
    fn name() -> &'static str {
        "string-knob-probe"
    }
}

impl crate::strategies::EngineStrategy for StringKnobStrategy {
    fn name(&self) -> &str {
        Self::name()
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn find_candidates(
        &mut self,
        _ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        Vec::new()
    }
    fn set_string_params(&mut self, overrides: &[(String, String)]) {
        self.seen
            .lock()
            .expect("lock")
            .extend(overrides.iter().cloned());
    }
    fn string_tunable_names(&self) -> Vec<String> {
        vec!["spot_missing".into()]
    }
    fn evolvable_knobs(&self) -> Vec<strategy_logic::params::KnobSpec> {
        vec![strategy_logic::params::KnobSpec::new(
            "gate_threshold",
            rust_decimal_macros::dec!(0.5),
            rust_decimal_macros::dec!(0.0),
            rust_decimal_macros::dec!(1.0),
        )]
    }
}

/// `mad_dog:spot_missing=pass`: the word rides the params bag verbatim — the
/// strategy SEES the string, the Decimal registry never does, and the replay
/// runs to a report.
#[test]
fn string_tunable_rides_the_params_bag_verbatim() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut b = EventBacktester::new(
        BacktestConfig {
            core: base_core(None),
            tick_ms: 50,
            tail_ms: 0,
            hot_params: vec![(
                StringKnobStrategy::name().to_string(),
                "spot_missing".to_string(),
                "pass".to_string(),
            )],
            progress: None,
            mode: String::new(),
        },
        Box::new(VecSource::new(scenario_events(NOW))),
    );
    b.host_strategy(Box::new(StringKnobStrategy {
        seen: std::sync::Arc::clone(&seen),
    }));
    assert!(
        b.core_mut()
            .set_strategy_enabled(StringKnobStrategy::name(), true),
        "the hosted adapter registers under its name"
    );
    b.run().expect("a string override is not a decimal refusal");
    let heard = seen.lock().expect("lock").clone();
    assert_eq!(
        heard,
        vec![("spot_missing".to_string(), "pass".to_string())],
        "the string tunable reached the bag verbatim, not a decimal pipe"
    );
}

/// A decimal knob handed a word is refused at startup with the flag, value and
/// expected type named — the P0 rule a string channel must not erode (#228/
/// #441): `spread_arb:take_profit_pct=abc` never silently becomes a string.
#[test]
fn decimal_knob_refuses_a_word_loudly() {
    let mut b = EventBacktester::new(
        BacktestConfig {
            core: base_core(None),
            tick_ms: 50,
            tail_ms: 0,
            hot_params: vec![(
                StringKnobStrategy::name().to_string(),
                "gate_threshold".to_string(),
                "abc".to_string(),
            )],
            progress: None,
            mode: String::new(),
        },
        Box::new(VecSource::new(scenario_events(NOW))),
    );
    b.host_strategy(Box::new(StringKnobStrategy {
        seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }));
    assert!(
        b.core_mut()
            .set_strategy_enabled(StringKnobStrategy::name(), true),
        "the hosted adapter registers under its name"
    );
    let err = b
        .run()
        .expect_err("a word on a decimal knob must be a startup refusal");
    assert!(
        err.contains("gate_threshold") && err.contains("not a decimal"),
        "the refusal names the knob and the decimal expectation: {err}"
    );
}

/// An undeclared knob name must not ride the string channel by default: a
/// name the strategy never opened would otherwise enter the bag without any
/// declaration behind it.
#[test]
fn undeclared_knob_name_refuses() {
    let mut b = EventBacktester::new(
        BacktestConfig {
            core: base_core(None),
            tick_ms: 50,
            tail_ms: 0,
            hot_params: vec![(
                StringKnobStrategy::name().to_string(),
                "no_such_knob".to_string(),
                "1.5".to_string(),
            )],
            progress: None,
            mode: String::new(),
        },
        Box::new(VecSource::new(scenario_events(NOW))),
    );
    b.host_strategy(Box::new(StringKnobStrategy {
        seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }));
    assert!(
        b.core_mut()
            .set_strategy_enabled(StringKnobStrategy::name(), true),
        "the hosted adapter registers under its name"
    );
    let err = b
        .run()
        .expect_err("an undeclared knob must be a startup refusal");
    assert!(
        err.contains("no_such_knob"),
        "the refusal names the unknown knob: {err}"
    );
}

/// A `--backtest-knob` naming a strategy nothing hosted carries refuses too —
/// the counterfactual would otherwise be a silent no-op that reads as data.
#[test]
fn unknown_strategy_refuses() {
    let mut b = EventBacktester::new(
        BacktestConfig {
            core: base_core(None),
            tick_ms: 50,
            tail_ms: 0,
            hot_params: vec![(
                "absent-strategy".to_string(),
                "spot_missing".to_string(),
                "pass".to_string(),
            )],
            progress: None,
            mode: String::new(),
        },
        Box::new(VecSource::new(Vec::new())),
    );
    let err = b
        .run()
        .expect_err("an override with no application site must refuse");
    assert!(
        err.contains("absent-strategy"),
        "the refusal names the unhosted strategy: {err}"
    );
}
