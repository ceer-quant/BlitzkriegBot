//! P0 risk-gate acceptance tests — the ISSUE-level behaviour, not the unit.
//!
//! Four audit findings are pinned here end to end, through the same public
//! entry points the kernel uses at runtime (`Core::place`, `Core::flatten`,
//! `Core::tick`, `PositionManager::check_exits`), because the unit tests inside
//! `risk.rs` / `exit_policy.rs` / `position.rs` each cover only one layer of the
//! chain these bugs travelled through:
//!
//! * **#174** — the kill switch must freeze ENTRIES and never the way out of a
//!   position: the closing SELL has to pass the same risk gate that refuses the
//!   BUY, or a stop-limited loss silently becomes unlimited.
//! * **#177** — the protective stop must fire when the bid is gone, missing or
//!   dislocated on a real decline, and a genuine pin bar that IS withheld must
//!   leave a report behind instead of vanishing.
//! * **#173** — the daily-loss breaker must actually exist: a real day boundary,
//!   a loss that survives a restart, a cap relative to the account, and a trip
//!   the operator can see.
//! * **#202** — one order's notional must be bounded as a SHARE of the account,
//!   because a fixed lot is not a risk statement: on the live 4.8 USDC book the
//!   shipped 10-share band commits 4.00–6.00 USD per order (83%–125% of it).
//!   The cap rejects over-cap orders and still lets every close through.

use blitzkrieg_core::ipc::schema::Event;
use blitzkrieg_core::model::{
    CoreErrorCode, ExitReason, FillPolicy, OrderRequest, OrderRole, OrderStatus, OrderbookSnapshot,
    Side, SignalDirection,
};
use blitzkrieg_core::position::{
    OpenParams, PositionConfig, PositionManager, StopSuppressionCause, utc_day_index,
};
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A fixed instant (2001-09-09, 01:46:40 UTC) whose UTC day index is 11_574 —
/// far from a boundary, so the tests can pick the day they mean.
const NOW: i64 = 1_000_000_000_000;
const DAY_MS: i64 = 86_400_000;

fn temp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("bk-risk-gates-{}-{name}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn dry_core(positions: PositionConfig) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        ..Default::default()
    });
    c.set_balance(dec!(100));
    c
}

/// The round slot the kernel derives from the clock: a position opened in slot N
/// expires at the END of it, so this keeps the test's expiry in the future (a
/// hard-coded slot puts it in 1970 and ForceExit preempts every other rule).
fn slot_at(now: i64) -> i64 {
    now / 1000 / 900
}

fn order(
    tok: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    key: &str,
    now: i64,
) -> OrderRequest {
    OrderRequest {
        token_id: tok.into(),
        condition_id: "cond".into(),
        side,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "s".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot_at(now),
    }
}

/// Open one LONG through the ordinary taker path (dry fills are immediate).
fn open_long(c: &mut Core, tok: &str, bid: Decimal, ask: Decimal, size: Decimal, now: i64) {
    c.book_snapshot(tok, vec![(bid, dec!(1000))], vec![(ask, dec!(1000))], now);
    let (_, status) = c
        .place(
            order(tok, Side::Buy, ask, size, &format!("entry:{tok}"), now),
            0,
            now,
        )
        .expect("the entry must be accepted");
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "one position must have opened"
    );
}

fn drain_alerts(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            out.push(message);
        }
    }
    out
}

// ── #174: the kill switch freezes entries, never the way out ────────────────

#[test]
fn the_kill_switch_freezes_entries_but_never_the_exit() {
    let mut c = dry_core(PositionConfig::default());
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(10), NOW);
    c.kill("test: venue refusals".into());
    assert!(c.is_killed());

    // (a) A new entry is frozen, with the kill switch's own error code.
    let err = c
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 1_000,
            ),
            0,
            NOW + 1_000,
        )
        .expect_err("a killed core must refuse new exposure");
    assert_eq!(err.code, CoreErrorCode::KillSwitchActive);

    // (b) The exemption is for CLOSING INTENTS, not for the SELL side: an
    // ordinary sell intent is still blocked, so the hatch cannot be abused to
    // hand exposure to someone else's key.
    let err = c
        .place(
            order(
                "tok",
                Side::Sell,
                dec!(0.38),
                dec!(10),
                "k-other",
                NOW + 2_000,
            ),
            0,
            NOW + 2_000,
        )
        .expect_err("a non-closing sell must stay blocked while killed");
    assert_eq!(err.code, CoreErrorCode::KillSwitchActive);

    // (c) The real way out — the operator's flatten — goes through and fills.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.38), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + 3_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 3_000).unwrap(),
        1,
        "the closing sell must be submitted"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the closing sell must have completed, not just been accepted"
    );
    // …and it must not have lifted the freeze it was exempt from.
    assert!(c.is_killed(), "closing a position is not a resume");
}

// ── #177: no-wick / missing-book stop, plus the withheld-trigger report ─────

#[test]
fn a_pin_bar_is_withheld_and_reported_then_a_real_decline_fires_the_stop() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = dry_core(PositionConfig::default());
    c.set_event_sink(tx);
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(10), NOW);
    let _ = drain_alerts(&mut rx); // the round-start chatter is not the subject

    // A pin bar 5s in: the bid craters to -50% while the mid holds at entry.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.20), dec!(1000))],
        vec![(dec!(0.60), dec!(1000))],
        NOW + 5_000,
    );
    c.tick(NOW + 5_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "a pin bar must not close the position"
    );
    assert!(
        !c.list_orders()
            .iter()
            .any(|o| o.side == Side::Sell && o.status.is_live()),
        "no closing sell may be resting off a pin bar"
    );
    // …but it must be REPORTED: "should have triggered" is an event, not a
    // silent no-op.
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("SUPPRESSED") && m.contains("wick guard")),
        "the withheld stop must reach the event sink: {alerts:?}"
    );
    assert_eq!(c.positions().suppressed_stop_count(), 1);
    let stats = c.engine_stats_at(NOW + 5_000);
    assert_eq!(stats["dailyLoss"]["suppressedStops"].as_u64(), Some(1));

    // 5s later the decline is real: the bid AND the mid collapse together, so
    // no wick ratio can excuse holding through it.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 10_000,
    );
    c.tick(NOW + 10_000).unwrap();
    assert!(
        c.positions().open_positions().is_empty(),
        "a real decline must fire the stop"
    );
}

fn book(
    bids: Vec<(Decimal, Decimal)>,
    asks: Vec<(Decimal, Decimal)>,
    now: i64,
) -> OrderbookSnapshot {
    // Built the way the kernel builds it from the mirrored book, so the
    // placeholder prices an empty side gets are part of the test.
    OrderbookSnapshot::from_levels("tok", bids, asks, now)
}

/// Open one LONG directly in the position manager (no Core), so a test can
/// hand it any book it likes — including none at all.
fn position(
    pm: &mut PositionManager,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    now: i64,
) -> String {
    let pos = pm.open(
        OpenParams {
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: tok.into(),
            condition_id: "cond".into(),
            entry_price: entry,
            expires_at_ms: now + 900_000,
            was_maker: false,
            target_exit_price: None,
        },
        now,
    );
    pm.apply_entry_fill(&pos.id, shares, entry, Decimal::ZERO, OrderRole::Taker);
    pos.id
}

#[test]
fn the_stop_survives_a_dead_quote_a_missing_book_and_a_collapsed_mid() {
    // (a) No bid at all (the book was swept). A one-sided book now has no mid
    // either — `from_levels` zeroes it, because `(0 + ask)/2` is an arithmetic
    // artifact, not a level anyone will lift. So there is no buyer to sell to
    // AND no trustworthy price to judge the stop on. F6's answer, and the
    // resolution of #179: hold. Minting a SELL out of the phantom mid is what
    // made the old code "book profit no buyer was offering".
    //
    // The visibility cost of this (a swept book leaves the stop unjudgeable,
    // because `stop_reference` only consults a mid that `from_levels` now
    // zeroes) is tracked as #225 — it is an alerting gap, not a money gap: the
    // position could not have been sold at any price either way.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| Some(book(vec![], vec![(dec!(0.15), dec!(1000))], NOW + 5_000)),
        NOW + 5_000,
    );
    assert!(
        reqs.is_empty(),
        "a bid-less book must never mint a SELL out of the mid or the ask"
    );

    // (b) No book at all, but a FRESH last valid price: the stop's judgement
    // stands — the position is underwater and the rule says get out — but a
    // price is not a bid, so the judgement still cannot become an order. No
    // order, AND no silence: the held exit is reported.
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    pm.tick(
        &id,
        Some(&book(
            vec![(dec!(0.15), dec!(1000))],
            vec![(dec!(0.16), dec!(1000))],
            NOW + 3_000,
        )),
        NOW + 3_000,
    );
    let reqs = pm.check_exits(&|_| None, NOW + 4_000);
    assert!(
        reqs.is_empty(),
        "a missing book must not mint a SELL either"
    );
    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "the held stop must be reported, not swallowed"
    );
    assert_eq!(events[0].cause, StopSuppressionCause::NoExecutableQuote);
    assert_eq!(events[0].bid, Decimal::ZERO, "there was no bid to report");
    assert_eq!(
        events[0].mid,
        dec!(0.15),
        "the last known price is what the stop judged on"
    );
    assert_eq!(events[0].pnl_pct_at_mid, dec!(-62.5));
    assert!(events[0].message().contains("no executable bid"));
    assert_eq!(
        pm.suppressed_stop_count(),
        1,
        "the panel counter moves for a held stop too"
    );
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a drain must be a drain"
    );

    // (b') …but the same price, aged past the freshness bound, must not: a
    // stale quote cannot speak for the market.
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    pm.tick(
        &id,
        Some(&book(
            vec![(dec!(0.15), dec!(1000))],
            vec![(dec!(0.16), dec!(1000))],
            NOW + 3_000,
        )),
        NOW + 3_000,
    );
    let stale_at = NOW + 3_000 + 31_000;
    assert!(
        pm.check_exits(&|_| None, stale_at).is_empty(),
        "a last price older than the bound must not fire the stop"
    );

    // (c) A real decline that ALSO trips the wick ratio: the bid hangs at 0.05
    // against a 0.075 ask (20% wick) but the MID collapsed with it, so the
    // guard has nothing to excuse and the stop fires.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(book(
                vec![(dec!(0.05), dec!(1000))],
                vec![(dec!(0.075), dec!(1000))],
                NOW + 5_000,
            ))
        },
        NOW + 5_000,
    );
    assert_eq!(reqs.len(), 1, "a collapsed mid outranks the wick guard");
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
}

#[test]
fn a_genuine_pin_bar_is_withheld_and_leaves_a_report() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(book(
                vec![(dec!(0.20), dec!(1000))],
                vec![(dec!(0.60), dec!(1000))],
                NOW + 5_000,
            ))
        },
        NOW + 5_000,
    );
    assert!(reqs.is_empty(), "a pin bar must not close the position");

    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "the withheld trigger must be reported once"
    );
    assert_eq!(events[0].position_id, id);
    assert_eq!(events[0].bid, dec!(0.20));
    assert_eq!(events[0].mid, dec!(0.40));
    assert_eq!(events[0].pnl_pct_at_bid, dec!(-50));
    assert_eq!(events[0].pnl_pct_at_mid, Decimal::ZERO);
    assert_eq!(events[0].stop_pct, dec!(12));
    assert!(events[0].message().contains("SUPPRESSED"));
    assert_eq!(pm.suppressed_stop_count(), 1, "the panel counter moves too");
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a drain must be a drain"
    );
}

// ── #202: one order is bounded as a share of the account ────────────────────

/// A dry core on a chosen account size with the equity-relative per-order cap
/// armed (`pct`) or off (`0`). The absolute cap is deliberately wide so only the
/// relative one can bite.
fn account_core(balance: Decimal, pct: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            max_order_notional_pct: pct,
            ..Default::default()
        },
        dry_seed_balance: balance,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions: PositionConfig::default(),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

#[test]
fn the_relative_cap_bounds_any_order_but_never_the_way_out() {
    // The live account: 4.8 USDC, 20% per order = 0.96 USD.
    let mut c = account_core(dec!(4.8), dec!(20));
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );

    // The measured live ticket — 10 shares at 0.40 = 4.00 USD, 83% of this
    // account — is refused, and the refusal names the cap that bit. The old
    // 6.00 absolute bound waved exactly this order through.
    let err = c
        .place(
            order("tok", Side::Buy, dec!(0.40), dec!(10), "entry:tok", NOW),
            0,
            NOW,
        )
        .expect_err("an over-cap entry must be rejected, not truncated");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("equity cap"), "{}", err.message);
    assert!(
        c.positions().open_positions().is_empty(),
        "a refused entry leaves no position and no half-filled remnant"
    );

    // The largest whole ticket inside 0.96 is 2 shares (0.80): it fills, so the
    // escape hatch below has a real position to reduce.
    let (_, status) = c
        .place(
            order("tok", Side::Buy, dec!(0.40), dec!(2), "entry:tok", NOW),
            0,
            NOW,
        )
        .expect("the in-cap entry must be accepted");
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions().len(), 1);

    // The way out: 2 shares at a 0.59 bid is 1.18 USD — over the cap — and the
    // close must pass anyway, because a cap that traps a position is #174
    // reopened through another door.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.59), dec!(1000))],
        vec![(dec!(0.60), dec!(1000))],
        NOW + 5_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 5_000).unwrap(),
        1,
        "the closing sell must be submitted"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the close must have completed in full"
    );
}

#[test]
fn the_panel_states_one_orders_worst_case_and_the_cap_that_bounds_it() {
    // Armed: 20% of a 4.8 book = 0.96 USD, and that is the binding bound.
    let c = account_core(dec!(4.8), dec!(20));
    let stats = c.engine_stats_at(NOW);
    let sizing = &stats["sizing"];
    assert_eq!(sizing["equityUsd"].as_f64(), Some(4.8));
    assert_eq!(sizing["shareBandUsd"].as_f64(), Some(10.0));
    assert_eq!(sizing["maxOrderNotionalPct"].as_f64(), Some(20.0));
    assert_eq!(sizing["equityCapUsd"].as_f64(), Some(0.96));
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(0.96));
    let pct = sizing["worstCasePctOfEquity"].as_f64().unwrap();
    assert!(pct <= 20.0 + 1e-9, "worst case must fit the cap: {pct}%");
    assert_eq!(sizing["closesExemptFromEquityCap"].as_bool(), Some(true));

    // Unconfigured (the shipped default): the same account has NO bound as a
    // share of itself — 10 shares × 1.00 = 10.00 USD, 208% of the book. That is
    // the hole #202 is about, and the panel now states it instead of leaving it
    // to be re-derived from the flags.
    let c = account_core(dec!(4.8), Decimal::ZERO);
    let sizing = &c.engine_stats_at(NOW)["sizing"];
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(10.0));
    let pct = sizing["worstCasePctOfEquity"].as_f64().unwrap();
    assert!((pct - 208.333_333).abs() < 0.001, "{pct}%");
    assert_eq!(sizing["equityCapUsd"], serde_json::Value::Null);

    // The same knobs on a 480 USD account: the PERCENTAGE is the same, the money
    // is not — which is why an absolute bound cannot say this on every account.
    let c = account_core(dec!(480), dec!(20));
    let sizing = &c.engine_stats_at(NOW)["sizing"];
    assert_eq!(sizing["equityCapUsd"].as_f64(), Some(96.0));
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(10.0));
}

// ── #173: the daily-loss breaker ────────────────────────────────────────────

/// The budget config the position-level tests use: every cooldown off, so the
/// only thing `can_open` can complain about is the daily cap.
fn budget_config(abs: Decimal, pct: Decimal, path: Option<String>) -> PositionConfig {
    PositionConfig {
        max_positions: 8,
        max_daily_loss_usd: abs,
        max_daily_loss_equity_pct: pct,
        daily_pnl_path: path,
        stop_loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        ..Default::default()
    }
}

/// Open `shares` at `entry` and close them at `exit`; returns the realized net
/// PnL the daily budget just absorbed.
fn realize(
    pm: &mut PositionManager,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    exit: Decimal,
    now: i64,
) -> Decimal {
    let id = position(pm, tok, entry, shares, now);
    pm.close(&id, exit, ExitReason::StopLoss, false, now + 1_000)
        .expect("the position is open")
        .net_pnl_usd
}

#[test]
fn the_cap_is_a_share_of_the_days_opening_equity_and_resets_only_across_days() {
    let mut pm = PositionManager::new(budget_config(Decimal::ZERO, dec!(20), None));
    let roll = pm
        .roll_daily(NOW, dec!(100))
        .expect("the first tick stamps the day");
    assert_eq!(roll.day_index, utc_day_index(NOW));
    assert_eq!(roll.limit_usd, dec!(20), "20% of the $100 opening equity");

    // A 15 USD loss is inside the budget: entries stay open.
    let net = realize(
        &mut pm,
        "tok",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(net < dec!(-10), "expected a real loss, got {net}");
    assert!(!pm.daily_loss_tripped());
    assert!(pm.can_open(None, None, NOW + 2_000).is_ok());

    // A second one crosses it, and the freeze is immediate.
    realize(
        &mut pm,
        "tok2",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 3_000,
    );
    assert!(pm.daily_loss_tripped(), "the cap must latch");
    let err = pm
        .can_open(None, None, NOW + 4_000)
        .expect_err("a tripped cap must refuse new entries");
    assert!(err.contains("Daily loss limit"), "{err}");

    // Same day: a later tick does not clear it.
    assert!(pm.roll_daily(NOW + 60_000, dec!(70)).is_none());
    assert!(pm.daily_loss_tripped());

    // A real UTC day boundary does — and re-bases the cap on the new equity.
    let roll = pm
        .roll_daily(NOW + DAY_MS, dec!(70))
        .expect("crossing midnight UTC must roll the budget");
    assert_eq!(roll.previous_day_index, Some(utc_day_index(NOW)));
    assert_eq!(roll.day_index, utc_day_index(NOW + DAY_MS));
    assert!(roll.previous_tripped, "the closed day's state is reported");
    assert!(roll.previous_realized_pnl_usd < dec!(-20));
    assert_eq!(roll.limit_usd, dec!(14), "20% of the new $70 equity");
    assert_eq!(pm.daily_pnl(), Decimal::ZERO);
    assert!(!pm.daily_loss_tripped(), "the new day starts clean");
    assert!(pm.can_open(None, None, NOW + DAY_MS + 1_000).is_ok());
}

#[test]
fn the_absolute_cap_still_wins_when_it_is_the_tighter_one() {
    // Absolute $5 against a 50% relative cap on $100 → $5.
    let mut pm = PositionManager::new(budget_config(dec!(5), dec!(50), None));
    pm.roll_daily(NOW, dec!(100));
    assert_eq!(pm.effective_daily_loss_limit(), dec!(5));

    // …and a generous absolute cap never raises the relative one.
    let mut pm = PositionManager::new(budget_config(dec!(500), dec!(10), None));
    pm.roll_daily(NOW, dec!(100));
    assert_eq!(pm.effective_daily_loss_limit(), dec!(10));
}

#[test]
fn a_restart_keeps_the_days_loss_and_the_next_day_still_clears_it() {
    let path = temp_path("daily-budget");
    let cfg = budget_config(
        dec!(1),
        Decimal::ZERO,
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily(NOW, dec!(100));
    let net = realize(
        &mut first,
        "tok",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(net < dec!(-1), "expected a loss past the cap, got {net}");
    assert!(first.daily_loss_tripped());
    assert!(
        path.exists(),
        "the day's budget must be on disk, not only in memory"
    );
    drop(first);

    // A fresh process (same file) must not be able to launder today's loss.
    let mut second = PositionManager::new(cfg);
    assert_eq!(second.daily_state().day_index, Some(utc_day_index(NOW)));
    assert!(
        second.daily_loss_tripped(),
        "the restored budget is already spent"
    );
    let err = second
        .can_open(None, None, NOW + 2_000)
        .expect_err("a restored loss must gate the very first entry");
    assert!(err.contains("Daily loss limit"), "{err}");

    // The boundary still lifts it, restored budget and all.
    let roll = second
        .roll_daily(NOW + DAY_MS, dec!(85))
        .expect("the next UTC day resets it");
    assert!(roll.previous_tripped);
    assert!(!second.daily_loss_tripped());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_trip_reaches_the_panel_and_the_audit_trail() {
    let path = temp_path("trip-alert");
    let positions = budget_config(
        dec!(1),
        Decimal::ZERO,
        Some(path.to_string_lossy().into_owned()),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = dry_core(positions.clone());
    c.set_event_sink(tx);
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(50), NOW);
    let _ = drain_alerts(&mut rx);

    // Crash the book: the stop exits at ~0.05 and realizes a loss past $1.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 5_000,
    );
    c.tick(NOW + 5_000).unwrap();

    assert!(c.positions().daily_pnl() < dec!(-1));
    assert!(c.positions().daily_loss_tripped());
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("DAILY LOSS LIMIT REACHED")),
        "the trip must be visible: {alerts:?}"
    );
    let stats = c.engine_stats_at(NOW + 5_000);
    assert_eq!(stats["dailyLoss"]["tripped"].as_bool(), Some(true));
    // The panel serializes decimals as strings (crate::decimal), so compare
    // the rendered value rather than its JSON variant.
    let realized = stats["dailyLoss"]["realizedPnlUsd"].to_string();
    assert!(
        realized.trim_matches('"').starts_with('-'),
        "the panel must carry the loss: {realized}"
    );
    assert_eq!(
        stats["dailyLoss"]["dayIndex"].as_i64(),
        Some(utc_day_index(NOW))
    );

    // A new entry is refused for the rest of the day…
    let err = c
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 6_000,
            ),
            0,
            NOW + 6_000,
        )
        .expect_err("the day's cap is spent");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // …and a restart does not give the day a second budget.
    let mut restarted = dry_core(positions);
    assert!(restarted.positions().daily_loss_tripped());
    let err = restarted
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 60_000,
            ),
            0,
            NOW + 60_000,
        )
        .expect_err("a restart must not launder today's loss");
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // Crossing the boundary re-opens entries, and the roll is announced.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    restarted.set_event_sink(tx2);
    restarted.tick(NOW + DAY_MS).unwrap();
    assert!(!restarted.positions().daily_loss_tripped());
    let alerts = drain_alerts(&mut rx2);
    assert!(
        alerts.iter().any(|m| m.contains("daily loss budget")),
        "the roll must be in the audit trail: {alerts:?}"
    );
    restarted.book_snapshot(
        "tok2",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + DAY_MS + 1_000,
    );
    let (_, status) = restarted
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + DAY_MS + 1_000,
            ),
            0,
            NOW + DAY_MS + 1_000,
        )
        .expect("the new day's entries are open again");
    assert_eq!(status, OrderStatus::Filled);
    let _ = std::fs::remove_file(&path);
}
