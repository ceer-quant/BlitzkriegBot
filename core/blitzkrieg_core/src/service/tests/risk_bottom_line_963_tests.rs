#![cfg(test)]

//! issue #963 — the kernel's risk BOTTOM LINE, proven by fault injection:
//! even when the entry signals are garbage (random, out of order, fired at
//! whatever price), every hard limit inside the kernel must hold, and exit
//! signals must win the same-cycle race against entries.
//!
//! The four proofs, mapped to the issue's acceptance list:
//!  1. `random_entry_fault_injection_*` — a deterministic pseudo-random
//!     signal storm under the #351 250 ms fill-model latency: every closed
//!     trade's realized stop loss stays inside `max_single_loss_usd`, and
//!     the daily-loss breaker trips loudly (RiskAlert + can_open refusal).
//!  2. `exit_maker_close_escalates_at_the_exit_clock` and friends — the exit
//!     path's own maker→taker escalation (the #963 gap: exits rested as pure
//!     makers and NEVER escalated, while entries had the 5 s entry clock).
//!  3. `strategy_exit_intent_beats_the_same_cycle_entry` — an exit signal and
//!     an entry born in the SAME engine cycle: the close submits first.
//!  4. `entry_caps_never_trap_the_exit_escalation` — negative direction: the
//!     entry-side caps must refuse entries but must never block the way OUT.

use super::*;
use crate::engine::{DataEvent, Engine, EngineConfig};
use crate::exit_policy::ExitConfig;
use crate::position::PositionConfig;
use crate::risk::RiskConfig;
use crate::risk::limits::{Bound, LimitSource};
use crate::test_fixtures::round_market;
use rust_decimal_macros::dec;
use std::collections::HashMap;

// ── shared fixtures ─────────────────────────────────────────────────────────

/// Dry core with automated exits on and a far-away round (the fault runs live
/// inside the round; nothing expires mid-test). `exit` patches the exit config.
fn fault_core(exit: impl FnOnce(&mut ExitConfig)) -> Core {
    let mut cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: true,
        round_duration_sec: 3_600,
        // The #351 dial the acceptance names: every taker leg — entry AND
        // stop — reaches the venue 250 ms late and reports 250 ms after that.
        fill_model: crate::sim::FillModel {
            taker_latency_ms: 250,
            taker_rtt_ms: 250,
            ..Default::default()
        },
        // Realized-only daily budget and NO cooldowns: the fault run wants
        // back-to-back trades — the shipped net-mark breaker (#464) and the
        // post-stop cooldowns would both pace the storm instead of testing it.
        // The $2 daily budget is set HERE, in the constructed config: a later
        // `config_mut` write would not reach the PositionManager's own copy.
        positions: PositionConfig {
            max_daily_loss_usd: dec!(2),
            daily_loss_marks_unrealized: false,
            stop_loss_cooldown_sec: 0,
            exit_cooldown_sec: 0,
            loss_cooldown_sec: 0,
            asset_cooldown_sec: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    exit(&mut cfg.positions.exit);
    let mut c = Core::new(cfg);
    c.set_balance(dec!(1000));
    c
}

fn fault_order(price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    fault_order_on("tok", "BTC", "up", price, size, key)
}

fn fault_order_on(
    token: &str,
    asset: &str,
    direction: &str,
    price: Decimal,
    size: Decimal,
    key: &str,
) -> OrderRequest {
    OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: asset.into(),
        direction: direction.into(),
        round_slot: 0,
        account_id: crate::model::default_account_id(),
    }
}

/// Open one position at `price` off a resting ask, deterministic. Under the
/// 250 ms fill model the entry leg is IN FLIGHT after `place`, so the fixture
/// advances the clock to its report before returning.
fn open_at(c: &mut Core, price: Decimal, size: Decimal, at_ms: i64) {
    c.book_snapshot("tok", vec![], vec![(price, dec!(500))], at_ms - 1);
    let (id, _) = c
        .place(
            fault_order(price, size, &format!("seed-{}", price)),
            0,
            at_ms,
        )
        .unwrap();
    c.tick(at_ms + 250).unwrap();
    c.tick(at_ms + 500).unwrap();
    assert_eq!(
        c.ome().get(&id).map(|o| o.status),
        Some(OrderStatus::Filled),
        "the seed entry must fill"
    );
}

/// A deterministic xorshift* — the "random" signal storm must replay
/// bit-identically, so the PRNG is a fixed-seed pure function, never the wall
/// clock and never an external crate.
struct FaultRng(u64);
impl FaultRng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

// ── acceptance 1: fault injection — random signals + 250 ms latency ─────────

/// Random / out-of-order entry signals (the operator's "入场信号完全是垃圾"
/// posture) fired against a book that crashes one tick per cycle, under the
/// 250 ms fill model and an armed per-account single-loss cap of $0.50 plus a
/// $1.50 daily budget. Every closed trade's realized STOP loss must stay at
/// or under the cap (the cap's own contract: worst case entry → bound stop,
/// measured at the venue's tick resolution), and the daily breaker must trip
/// loudly: a RiskAlert, the sticky latch, and a can_open refusal naming it.
#[test]
fn random_entry_fault_injection_single_loss_cap_holds_and_daily_breaker_trips() {
    let mut c = fault_core(|exit| {
        // The shipped protective side only: 12% base stop, ramp floor 10%.
        // The single-loss cap sizes against the BASE (the #963 widening) and
        // the tick-realizable fill (the #963 grid bound), so every realized
        // stop stays inside the cap by construction.
        *exit = ExitConfig::default();
    });
    c.risk.config_mut().systemic.account.max_single_loss_usd =
        Bound::new(dec!(0.5), LimitSource::Env);
    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    // Entry prices land their 12% stop exactly on the venue's 0.01 grid
    // (0.25 → 0.22, 0.50 → 0.44, 0.75 → 0.66), so the realized stop loss is
    // the cap's own continuous number with no tick residue: the assertion
    // below is the EXACT cap, not a slippage-tolerant one.
    let prices = [dec!(0.25), dec!(0.50), dec!(0.75)];
    let cap = dec!(0.5);
    let mut rng = FaultRng(0x963);
    let mut t = 1_000i64;
    let mut closed_seen = 0usize;
    let mut trip_seen = false;
    let mut breaker_seen = 0usize;
    let mut alert_log: Vec<String> = Vec::new();

    for cycle in 0..150 {
        t += 500;
        // The crash: one tick per cycle — never a gap through the stop (a
        // gap is a market-structure failure the kernel REPORTS, not a bound
        // it can price; see the review doc). A sawtooth: the wave resets to
        // the top so every price band's stop gets its turn to fire. Books
        // stay fresh every cycle so the freshness gates never withhold a
        // stop.
        let step = (cycle % 80) as i64;
        let bid = dec!(0.76) - Decimal::from(step) * dec!(0.01);
        let ask = bid + dec!(0.01);
        c.book_snapshot("tok", vec![(bid, dec!(500))], vec![(ask, dec!(500))], t);

        // The garbage signal: random price from the grid-exact set, random
        // size, random key, one of four strategy names — re-emitted every
        // cycle, out of order, with no strategy state at all. The rotation
        // matters: the shipped KI-10 breaker (3 consecutive losses → 300 s)
        // halts each NAME independently, and the storm must keep exposing
        // the daily budget through the names that are still live. Every gate
        // downstream must hold the line.
        let price = prices[(rng.next() % 3) as usize];
        let size = Decimal::from(1 + rng.next() % 20);
        let strategy = format!("fault{}", cycle % 4);
        let key = format!("fault-{cycle}");
        let mut req = fault_order(price, size, &key);
        req.strategy = strategy;
        match c.place(req, 0, t) {
            Err(e) if e.message.contains("breaker active") => breaker_seen += 1,
            _ => {}
        }

        // The #351 clock: match at +250, book at +500 — the maintenance tick
        // advances the in-flight legs and runs the exit ladder each beat.
        c.tick(t + 250).unwrap();
        c.tick(t + 500).unwrap();

        // Acceptance 1a: every CLOSED trade's stop loss stays inside the cap.
        for closed in c.positions().closed_positions().iter().skip(closed_seen) {
            closed_seen += 1;
            let price_loss = (closed.entry_price - closed.exit_price) * closed.shares;
            assert!(
                price_loss <= cap,
                "closed trade {} lost {price_loss} > cap {cap} (entry {}, exit {}, {} shares)",
                closed.id,
                closed.entry_price,
                closed.exit_price,
                closed.shares
            );
            // The cash the ledger actually moved stays bounded too: the price
            // loss plus the trade's OWN recorded fees, nothing else.
            let fees = closed.entry_fee_pct * closed.cost_usd / dec!(100)
                + closed.exit_fee_pct * (closed.exit_price * closed.shares) / dec!(100);
            assert!(
                closed.net_pnl_usd >= -(cap + fees),
                "net {} breached cap+fees {}",
                closed.net_pnl_usd,
                cap + fees
            );
        }
        while let Ok(ev) = rx.try_recv() {
            if let Event::RiskAlert { message, .. } = ev {
                alert_log.push(message);
            }
        }

        // Acceptance 1b: the breaker trip is OBSERVABLE — event, latch, gate.
        if !trip_seen && c.positions().daily_loss_tripped_for("default") {
            trip_seen = true;
            assert!(
                alert_log.iter().any(|m| m.contains("DAILY LOSS LIMIT")),
                "the daily trip must announce itself on the event stream"
            );
        }
    }

    assert!(
        closed_seen >= 3,
        "the storm must close trades, got {closed_seen}"
    );
    assert!(
        breaker_seen > 0,
        "the KI-10 loss breaker must fire during the storm (observability)"
    );
    assert!(
        trip_seen,
        "the daily breaker must trip inside the fault run"
    );
    assert!(
        c.positions().daily_loss_tripped_for("default"),
        "the trip latch is sticky for the day"
    );
    // And the tripped breaker REFUSES the next entry by name.
    let err = c
        .place(fault_order(dec!(0.5), dec!(1), "post-trip"), 0, t + 1_000)
        .unwrap_err();
    assert!(
        err.message.contains("Daily loss limit"),
        "post-trip entry must name the daily breaker, got {err}"
    );
}

// ── acceptance 2: the exit's own maker→taker escalation clock ────────────────

/// The #963 gap: a maker-first exit (TP class, the shipped
/// `maker_first_exit_enabled = true`) rested as a pure `FillPolicy::Maker`
/// and NEVER escalated — no deadline at all, while entries waited at most the
/// 5 s entry clock. Now it rests as `MakerThenTaker` under
/// `exit_maker_timeout_ms` (shipped 250 ms): still resting first, with a
/// deadline. The escalation fires exactly at the exit clock and the close
/// crosses at the book.
#[test]
fn exit_maker_close_escalates_at_the_exit_clock() {
    let mut c = fault_core(|exit| {
        exit.strategy_exit_maker_first = true;
        exit.exit_maker_timeout_ms = 250;
    });
    open_at(&mut c, dec!(0.40), dec!(10), 1_000);
    // In profit, one-tick spread: the maker close rests INSIDE (0.41).
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: true,
            exit_price: None,
        });
    c.tick(2_000).unwrap();

    let rested = c
        .ome()
        .live_orders()
        .into_iter()
        .find(|o| o.side == Side::Sell)
        .expect("the maker close rests");
    assert_eq!(rested.mode, FillPolicy::MakerThenTaker);
    assert_eq!(
        rested.escalate_at_ms,
        Some(2_250),
        "the exit's OWN clock, not the 5 s entry default"
    );

    // One millisecond before the deadline: still resting.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_240,
    );
    c.tick(2_240).unwrap();
    assert!(
        c.ome().live_orders().iter().any(|o| o.side == Side::Sell),
        "the close has not reached its clock yet"
    );

    // At the deadline the escalation cancels the maker and crosses: the
    // identity fill model walks the 0.40 bid and the position closes.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_250,
    );
    c.tick(2_250).unwrap();
    // The escalated taker is in flight under the 250 ms fill model: match at
    // 2_500, report at 2_750.
    c.tick(2_500).unwrap();
    c.tick(2_750).unwrap();
    assert!(
        c.positions().open_positions().is_empty(),
        "the escalated close must fill, not keep resting"
    );
    assert_eq!(
        c.positions().closed_positions()[0].exit_reason,
        ExitReason::TakeProfit
    );
    assert!(
        !c.ome().live_orders().iter().any(|o| o.side == Side::Sell),
        "no exit order lingers"
    );
}

/// The operator's off switch: a NEGATIVE exit timeout is the pure-maker
/// posture — the close rests past even the 5 s entry default and never
/// escalates (the same three-valued convention the entry clock spells).
#[test]
fn exit_maker_close_with_negative_timeout_never_escalates() {
    let mut c = fault_core(|exit| {
        exit.strategy_exit_maker_first = true;
        exit.exit_maker_timeout_ms = -1;
    });
    open_at(&mut c, dec!(0.40), dec!(10), 1_000);
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: true,
            exit_price: None,
        });
    c.tick(2_000).unwrap();
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        9_000,
    );
    c.tick(9_000).unwrap();
    let rested = c
        .ome()
        .live_orders()
        .into_iter()
        .find(|o| o.side == Side::Sell)
        .expect("the pure-maker close still rests");
    assert_eq!(rested.escalate_at_ms, None, "negative timeout = never");
}

/// Negative direction (the hard limit must not become a trap): with the
/// per-account entry caps armed to values that REFUSE any entry, the exit
/// escalation still fires and still closes. The caps bound NEW exposure —
/// never the way out (#174 doctrine, now holding through the escalation
/// guard too, whose systemic block is close-aware since #963).
#[test]
fn entry_caps_never_trap_the_exit_escalation() {
    let mut c = fault_core(|exit| {
        exit.strategy_exit_maker_first = true;
        exit.exit_maker_timeout_ms = 250;
    });
    open_at(&mut c, dec!(0.40), dec!(10), 1_000);
    // Arms that refuse EVERY entry: no whole share fits the share cap, and
    // the single-loss cap cannot fit one share's stop either.
    c.risk.config_mut().systemic.account.max_position_size =
        Bound::new(dec!(0.5), LimitSource::Env);
    c.risk.config_mut().systemic.account.max_single_loss_usd =
        Bound::new(dec!(0.01), LimitSource::Env);
    let refused = c
        .place(fault_order(dec!(0.40), dec!(1), "capped-entry"), 0, 2_000)
        .unwrap_err();
    assert!(
        refused.message.contains("fewer than one whole share fits"),
        "the control: the caps refuse entries, got {refused}"
    );

    // The exit, under the same caps: rests, escalates at the exit clock,
    // fills, closes. Never trapped.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_000,
    );
    c.strategy_exits
        .push(crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: true,
            exit_price: None,
        });
    c.tick(2_000).unwrap();
    c.book_snapshot(
        "tok",
        vec![(dec!(0.40), dec!(500))],
        vec![(dec!(0.44), dec!(500))],
        2_250,
    );
    c.tick(2_250).unwrap();
    c.tick(2_500).unwrap();
    c.tick(2_750).unwrap();
    assert!(
        c.positions().open_positions().is_empty(),
        "the exit escalation must not be blocked by entry-side caps"
    );
}

// ── acceptance 3: same-cycle exit priority ───────────────────────────────────

/// A strategy that emits an exit intent AND an entry candidate in the SAME
/// engine cycle — the exact race the issue names.
struct RaceStrategy {
    entry_emitted: bool,
    exit_emitted: bool,
}
impl crate::strategies::EngineStrategy for RaceStrategy {
    fn name(&self) -> &str {
        "race"
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    // The entry-quality gates (timing/spot momentum) are not what this test
    // exercises — the RACE is. Every structural and safety gate downstream
    // (risk, capacity, dedup, sizing) still runs untouched.
    fn gate_exemptions(&self) -> crate::strategies::GateExemptions {
        crate::strategies::GateExemptions {
            timing: true,
            momentum: true,
            timing_min_time_left_sec: None,
        }
    }
    fn find_candidates(
        &mut self,
        _ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        if self.entry_emitted {
            return Vec::new();
        }
        self.entry_emitted = true;
        vec![crate::signal::TradeSignal {
            strategy: "race".into(),
            account_id: crate::model::default_account_id().to_string(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "up".into(),
            condition_id: "cond".into(),
            price: dec!(0.45),
            reason: "race entry".into(),
            shares: None,
            pair: None,
        }]
    }
    fn take_exit_intents(&mut self) -> Vec<crate::strategies::StrategyExitIntent> {
        if self.exit_emitted {
            return Vec::new();
        }
        self.exit_emitted = true;
        vec![crate::strategies::StrategyExitIntent {
            token_id: "tok".into(),
            reason: "take_profit".into(),
            account_id: crate::model::default_account_id().to_string(),
            use_maker: false,
            exit_price: None,
        }]
    }
}

fn race_engine() -> EngineConfig {
    EngineConfig {
        scanner: crate::scanner::ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        trend: crate::signal::TrendConfig {
            confirm_sec: 5,
            ratio: dec!(0.5),
            min_price: dec!(0.5),
            broken_price: dec!(0.35),
            window_floor_ms: 0,
        },
        spread_arb: crate::signal::SpreadArbConfig::default(),
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(4.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        size_pct: Decimal::ZERO,
        strategy_sizes: HashMap::new(),
    }
}

/// An exit signal and an entry born in the SAME engine cycle: the close
/// submits (and fills) BEFORE the entry is even attempted. Before #963 the
/// intent drained into `strategy_exits` and waited for the NEXT maintenance
/// pass, while the entry loop placed immediately — the entry always won the
/// same-cycle race. The order ids are minted in submission order and the
/// event stream is the timing evidence the acceptance asks for.
#[test]
fn strategy_exit_intent_beats_the_same_cycle_entry() {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        auto_exits_enabled: true,
        round_duration_sec: 900,
        ..Default::default()
    });
    c.set_balance(dec!(1000));
    c.enable_engine(Engine::new(race_engine()));
    c.engine
        .as_mut()
        .unwrap()
        .register_user_strategy(
            Box::new(RaceStrategy {
                entry_emitted: false,
                exit_emitted: false,
            }),
            "test".into(),
        )
        .unwrap();
    assert!(
        c.engine
            .as_mut()
            .unwrap()
            .set_strategy_enabled("race", true)
    );

    let now = 1_000i64;
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![round_market(now)],
            now_ms: now,
        },
        now,
    );
    // A held position on `tok` the exit intent can name, and a fresh bid to
    // close into; `up` carries an ask so the entry has somewhere to go.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], now);
    let (seed_id, _) = c
        .place(
            fault_order_on("tok", "ETH", "down", dec!(0.40), dec!(5), "seed"),
            0,
            now,
        )
        .unwrap();
    assert_eq!(
        c.ome().get(&seed_id).map(|o| o.status),
        Some(OrderStatus::Filled)
    );
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(500))],
        vec![(dec!(0.41), dec!(500))],
        now + 1,
    );
    c.book_snapshot("up", vec![], vec![(dec!(0.45), dec!(100))], now + 1);

    let (tx, mut rx) = mpsc::unbounded_channel();
    c.set_event_sink(tx);

    let placed = c.engine_evaluate(now + 1);
    assert_eq!(placed, 1, "the entry was attempted this cycle");

    // The close did not wait a pass: the TOK position it named is gone this
    // cycle. (The entry may well have OPENED its own position in the same
    // cycle — it placed after the close, which is exactly the point.)
    assert!(
        c.positions()
            .closed_positions()
            .iter()
            .any(|cl| cl.token_id == "tok" && cl.exit_reason == ExitReason::TakeProfit),
        "the exit intent must be consumed in the SAME cycle it was emitted"
    );

    // Timing evidence: order ids are minted in submission order, so the sell
    // carries a SMALLER id than the buy of the same cycle, both stamped the
    // same maintenance instant.
    let orders = c.ome().all();
    let sell = orders
        .iter()
        .find(|o| o.side == Side::Sell && o.internal_key.starts_with("exit-strategy:"))
        .expect("the close order exists");
    let buy = orders
        .iter()
        .find(|o| o.side == Side::Buy && o.strategy == "race")
        .expect("the entry order exists");
    let num = |id: &str| -> i64 {
        id.rsplit('_')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(i64::MAX)
    };
    assert_eq!(sell.submitted_at_ms, now + 1);
    assert_eq!(buy.submitted_at_ms, now + 1);
    assert!(
        num(&sell.order_id) < num(&buy.order_id),
        "the exit must submit BEFORE the same-cycle entry: sell {} vs buy {}",
        sell.order_id,
        buy.order_id
    );

    // The event stream says the same thing: the close's first event lands
    // before the entry's first event.
    let mut events: Vec<String> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::OrderUpdate { order } = ev {
            events.push(order.order_id);
        }
    }
    let first = |id: &str| events.iter().position(|e| e == id);
    match (first(&sell.order_id), first(&buy.order_id)) {
        (Some(s), Some(b)) => assert!(
            s < b,
            "the close's event must precede the entry's: {events:?}"
        ),
        (s, b) => panic!("both orders must appear on the stream: sell {s:?} buy {b:?}"),
    }
}

// ── acceptance 2 (unit): the escalated close keeps its close-intent status ──

/// The escalated taker of a resting CLOSE is re-keyed `exit-escalated:{id}` —
/// the close hatches (kill switch, family gate, the guard's systemic
/// exemption) must follow it; an entry leg's `{id}:escalated` must not gain
/// them.
#[test]
fn the_escalated_close_key_keeps_close_intent() {
    assert!(crate::risk::is_close_intent("exit:dry_5:TakeProfit"));
    assert!(crate::risk::is_close_intent("exit-escalated:dry_5"));
    assert!(!crate::risk::is_close_intent("dry_5:escalated"));
    assert!(!crate::risk::is_close_intent("spread_arb:BTC:up:1"));
}

// ── 2026-11-08 pre-release find: the withheld-exit log is a state change ───

/// A restored position whose venue book never freshens re-emits the strategy
/// exit intent every engine tick, and the "no fresh best bid" withhold used
/// to log an INFO line PER TICK — one boot produced ~2M lines at
/// hundreds/second until the expiry sweep closed the position. The withhold
/// itself is correct pricing discipline (never sell the mid); the LOG must
/// carry the state CHANGE: once when the withhold first hits the position,
/// silent on every repeat, news again once a fresh bid resolves it.
#[test]
fn withheld_exit_log_fires_once_per_state_change_not_per_tick() {
    let mut c = fault_core(|_| {});
    open_at(
        &mut c,
        rust_decimal_macros::dec!(0.40),
        rust_decimal_macros::dec!(2),
        1_000,
    );
    let pos_id = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == "tok")
        .expect("seed position open")
        .id
        .clone();
    let intent = crate::strategies::StrategyExitIntent {
        token_id: "tok".into(),
        reason: "tp".into(),
        account_id: crate::model::default_account_id().to_string(),
        use_maker: false,
        exit_price: None,
    };
    // Far past the seed book's trust window: the entry book is stale here.
    let now = 1_000_000;

    // Tick 1: withheld, logged (first news for this position).
    let jobs = c.exit_jobs_from_strategy_intents(
        vec![intent.clone()],
        &mut std::collections::HashSet::new(),
        now,
    );
    assert!(
        jobs.is_empty(),
        "no fresh bid → no job (len {})",
        jobs.len()
    );
    assert!(
        c.withheld_exit_logged.contains(&pos_id),
        "first withhold is logged (state change)"
    );

    // Tick 2..N: SAME withhold — the set already carries the position, so
    // every repeat stays silent (the fix: no per-tick line).
    let jobs = c.exit_jobs_from_strategy_intents(
        vec![intent.clone()],
        &mut std::collections::HashSet::new(),
        now,
    );
    assert!(jobs.is_empty());
    assert!(c.withheld_exit_logged.contains(&pos_id));

    // A fresh bid resolves the intent into a job — and clears the mark, so a
    // LATER withhold for the same position is news again.
    c.book_snapshot(
        "tok",
        vec![(
            rust_decimal_macros::dec!(0.50),
            rust_decimal_macros::dec!(500),
        )],
        vec![(
            rust_decimal_macros::dec!(0.51),
            rust_decimal_macros::dec!(500),
        )],
        now,
    );
    let jobs = c.exit_jobs_from_strategy_intents(
        vec![intent.clone()],
        &mut std::collections::HashSet::new(),
        now,
    );
    assert_eq!(
        jobs.len(),
        1,
        "a fresh bid resolves the exit (len {})",
        jobs.len()
    );
    assert!(
        !c.withheld_exit_logged.contains(&pos_id),
        "resolved exit clears the withheld mark"
    );

    // Book goes stale again: the withhold logs once more (new state change).
    let jobs = c.exit_jobs_from_strategy_intents(
        vec![intent],
        &mut std::collections::HashSet::new(),
        now + 1_000_000,
    );
    assert!(jobs.is_empty());
    assert!(
        c.withheld_exit_logged.contains(&pos_id),
        "a fresh withhold after resolution is news again"
    );
}
