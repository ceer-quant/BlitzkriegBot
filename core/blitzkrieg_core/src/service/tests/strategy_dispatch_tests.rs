#![cfg(test)]
//! P-1.1: host dispatch to the builtin strategy through Core, with per-strategy
//! caps and per-strategy accounting.

use super::shadow_evolution_tests::scratch;
use super::*;
use crate::engine::{DataEvent, Engine, EngineConfig};
use crate::model::CryptoMarket;
use crate::risk::RiskConfig;
use crate::shadow_evolution::MutableParams;
use crate::test_fixtures::round_market;
use rust_decimal_macros::dec;
use strategy_logic::params::StrategyParams;

/// Mirrors the production engine knobs closely enough to trade the feed below
/// (same shape as the engine.rs unit tests).
fn engine_cfg() -> EngineConfig {
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
        spread_arb: crate::signal::SpreadArbConfig {
            trend_max_entry_price: dec!(0.45),
            ..Default::default()
        },
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        size_pct: Decimal::ZERO,
        strategy_sizes: HashMap::new(),
    }
}

fn core_with_engine(limits: HashMap<String, StrategyLimit>) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        strategy_limits: limits,
        ..Default::default()
    });
    c.enable_engine(Engine::new(engine_cfg()));
    // PR-B: the kernel registers no strategies, so the dispatch tests that
    // drive the reference dip buyer host the test adapter explicitly.
    crate::strategies::test_support::host(
        c.engine.as_mut().unwrap(),
        engine_cfg().trend,
        engine_cfg().spread_arb,
    );
    c
}

// ── Replay fast path (--backtest-fast) gating semantics ────────────────

/// Counts REAL strategy evaluations (`find_candidates` calls) — the
/// observable behind the fast path's "a quiet tick skips the engine
/// call" claim, without hosting a Lua runtime.
struct ProbeCount {
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl crate::strategies::EngineStrategy for ProbeCount {
    fn name(&self) -> &str {
        "probe"
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn find_candidates(
        &mut self,
        _ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Vec::new()
    }
}

fn core_with_probe(fast: bool) -> (Core, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let mut c = Core::new(CoreConfig {
        engine_enabled: true,
        auto_exits_enabled: false,
        backtest_fast: fast,
        ..Default::default()
    });
    c.enable_engine(Engine::new(engine_cfg()));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    c.engine
        .as_mut()
        .unwrap()
        .register_user_strategy(
            Box::new(ProbeCount {
                count: count.clone(),
            }),
            "test".into(),
        )
        .expect("fresh engine registers the probe");
    assert!(
        c.engine
            .as_mut()
            .unwrap()
            .set_strategy_enabled("probe", true)
    );
    (c, count)
}

/// A test strategy that dips on an explicit asset list. Declaring the assets
/// lets several strategies share one cycle without fighting over a token.
struct TargetDip {
    name: String,
    buy_below: Decimal,
    assets: Vec<String>,
    /// Shared entry gates this strategy declares unnecessary (E2-b / #27).
    gates: crate::strategies::GateExemptions,
    /// THIS instance's registration account — the stamp the signal
    /// carries (packages are account-blind by declaration; the host
    /// builder passes the scope account the same way `build_lua_scope`
    /// does). Default scope keeps the process default.
    scope: String,
}
impl crate::strategies::EngineStrategy for TargetDip {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn gate_exemptions(&self) -> crate::strategies::GateExemptions {
        self.gates
    }
    fn instance_account(&self) -> String {
        self.scope.clone()
    }
    fn find_candidates(
        &mut self,
        ctx: &crate::strategies::StrategyCtx<'_>,
    ) -> Vec<crate::signal::TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            if !self.assets.contains(&market.asset) {
                continue;
            }
            for (token_id, direction) in [
                (&market.up_token_id, crate::model::SignalDirection::Up),
                (&market.down_token_id, crate::model::SignalDirection::Down),
            ] {
                let Some(book) = ctx.fresh_book(token_id) else {
                    continue;
                };
                if book.mid_price <= self.buy_below {
                    out.push(crate::signal::TradeSignal {
                        strategy: self.name.clone(),
                        account_id: self.scope.clone(),
                        asset: market.asset.clone(),
                        direction,
                        token_id: token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price: book.mid_price,
                        reason: "target dip".into(),
                        shares: None,
                        pair: None,
                    });
                }
            }
        }
        out
    }
}

const DIPS: [(&str, &[&str]); 3] = [("small", &["BTC"]), ("mid", &["ETH"]), ("unset", &["SOL"])];

/// Core with a three-asset engine and the given dip strategies registered
/// enabled. `spread_arb` is switched off so only the test strategies emit.
///
/// The engine is built through `CoreConfig::engine_config()` — the ONE
/// production mapping — so these tests exercise the same per-strategy sizing
/// projection the live server and the backtester use.
fn core_with_dips(
    limits: HashMap<String, StrategyLimit>,
    dips: &[(&str, &[&str])],
    global_max_positions: usize,
) -> Core {
    core_with_declared_dips(limits, dips, global_max_positions, 0, |_| {
        crate::strategies::GateExemptions::none()
    })
}

/// Same as [`core_with_dips`], but each strategy also declares the shared
/// entry gates it does not need (E2-b / #27) and the round-timing window is
/// set up front — the engine takes its scanner config from
/// `CoreConfig::engine_config()` at construction, so a later `config_mut`
/// write would not reach it (nor would one reach the `RiskGate`, which
/// snapshots `config.risk` the same way).
fn core_with_declared_dips(
    limits: HashMap<String, StrategyLimit>,
    dips: &[(&str, &[&str])],
    global_max_positions: usize,
    min_round_age_sec: i64,
    gates: impl Fn(&str) -> crate::strategies::GateExemptions,
) -> Core {
    core_with_declared_dips_and_risk(
        limits,
        dips,
        global_max_positions,
        min_round_age_sec,
        RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        gates,
    )
}

/// The full form: also chooses the risk config the gate is built from.
fn core_with_declared_dips_and_risk(
    limits: HashMap<String, StrategyLimit>,
    dips: &[(&str, &[&str])],
    global_max_positions: usize,
    min_round_age_sec: i64,
    risk: RiskConfig,
    gates: impl Fn(&str) -> crate::strategies::GateExemptions,
) -> Core {
    let mut c = Core::new(CoreConfig {
        risk,
        dry_seed_balance: dec!(1000),
        engine_enabled: true,
        round_duration_sec: 900,
        min_round_age_sec,
        auto_exits_enabled: false,
        assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
        positions: crate::position::PositionConfig {
            max_positions: global_max_positions,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        strategy_limits: limits,
        ..Default::default()
    });
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    // Host the reference adapters too (the kernel ships no strategies, PR-B),
    // so gate-exemption accounting has the same declarations to list the
    // production session would have.
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    for (name, assets) in dips {
        eng.register_user_strategy(
            Box::new(TargetDip {
                name: (*name).to_string(),
                buy_below: dec!(0.45),
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
                gates: gates(name),
                scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            }),
            "test".into(),
        )
        .unwrap();
        assert!(eng.set_strategy_enabled(name, true));
    }
    // No isolation step is needed any more: the kernel ships zero strategies
    // (PR-B), so a freshly built engine hosts exactly the dips registered above.
    c.enable_engine(eng);
    c
}

fn three_markets(now: i64) -> Vec<CryptoMarket> {
    let _ = now;
    ["BTC", "ETH", "SOL"]
        .iter()
        .map(|a| CryptoMarket {
            asset: (*a).into(),
            condition_id: format!("cond_{a}"),
            question_id: format!("q_{a}"),
            up_token_id: format!("{}_up", a.to_lowercase()),
            down_token_id: format!("{}_down", a.to_lowercase()),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: 1_800_000,
            round_slot: 1,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: format!("{a} up or down"),
        })
        .collect()
}

/// Push a 0.44-mid book on the UP token of each given asset. Only the listed
/// assets dip, so a cycle can be aimed at one strategy.
fn feed_dip_on(c: &mut Core, now: i64, assets: &[&str]) {
    for asset in assets {
        let token = format!("{}_up", asset.to_lowercase());
        c.engine_on_data(
            DataEvent::Book {
                token_id: token,
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now,
            },
            now,
        );
    }
}

/// Round + a 0.44-mid book on every asset's up token. Each strategy that
/// targets one of those assets emits one entry priced at 0.44.
fn feed_three_asset_dip(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    feed_dip_on(c, now + 1_000, &["BTC", "ETH", "SOL"]);
}

/// Cross the book so the resting maker bid fills and a position opens.
fn fill(c: &mut Core, asset: &str, now: i64) {
    let token = format!("{}_up", asset.to_lowercase());
    c.book_snapshot(
        &token,
        vec![(dec!(0.42), dec!(100))],
        vec![(dec!(0.42), dec!(100))],
        now,
    );
}

fn limits_of(rows: &[(&str, StrategyLimit)]) -> HashMap<String, StrategyLimit> {
    rows.iter()
        .map(|(n, l)| ((*n).to_string(), l.clone()))
        .collect()
}

/// One strategy's override bag for the startup-selection tests, built the way
/// the IPC handler builds it (the shadow-evolution test module has its own).
fn one_param(strategy: &str, knob: &str, value: Decimal) -> MutableParams {
    let mut p = StrategyParams::new();
    p.set(knob, value);
    let mut m = MutableParams::new();
    m.set_strategy(strategy, p);
    m
}

// ── #406: the zero-price fail-closed guard ──────────────────────────────

/// A one-asset dip core whose sizing is the production mapping
/// (`CoreConfig::engine_config()`), so the ticket a test reads is the ticket
/// the live server would emit on that account.
fn equity_core(balance: Decimal, size_pct: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: balance,
        engine_enabled: true,
        round_duration_sec: 900,
        auto_exits_enabled: false,
        size_usd: dec!(2.5),
        // The engine's own ceiling stays wide so the equity budget is what
        // decides the lot in these tests.
        min_shares: dec!(1),
        max_shares: dec!(1000),
        size_pct,
        assets: vec!["BTC".into()],
        positions: crate::position::PositionConfig {
            max_positions: 5,
            exit: crate::exit_policy::ExitConfig {
                min_time_left_sec: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    });
    c.set_balance(balance);
    let cfg = c.config().engine_config();
    let mut eng = Engine::new(cfg.clone());
    crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
    eng.register_user_strategy(
        Box::new(TargetDip {
            name: "dip".into(),
            buy_below: dec!(0.45),
            assets: vec!["BTC".into()],
            gates: crate::strategies::GateExemptions::none(),
            scope: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
        }),
        "test".into(),
    )
    .unwrap();
    assert!(eng.set_strategy_enabled("dip", true));
    c.enable_engine(eng);
    c
}

/// Round + a 0.44-mid book on BTC: the dip strategy's one candidate.
fn feed_btc_dip(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: three_markets(now),
            now_ms: now,
        },
        now,
    );
    feed_dip_on(c, now + 1_000, &["BTC"]);
}

/// Round + confirmed UP trend + calm spot + one dip book: the builtin emits
/// exactly one spread_arb entry (0.43 x 10 = 4.30 USD) on the next cycle.
fn feed_entry_setup(c: &mut Core, now: i64) {
    c.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![round_market(now)],
            now_ms: now,
        },
        now,
    );
    for i in 0..12 {
        let t = now + i * 1000;
        c.engine_on_data(
            DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: t,
            },
            t,
        );
    }
    let t = now + 12_000;
    c.engine_on_data(
        DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: t,
        },
        t,
    );
    c.engine_on_data(
        DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: t,
        },
        t,
    );
}

/// The payload decimals arrive as JSON numbers (Node wire convention).
fn dec_of(v: &serde_json::Value) -> Decimal {
    match v {
        serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()),
        serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()),
        other => panic!("not a decimal: {other}"),
    }
    .unwrap_or_else(|e| panic!("not a decimal: {v} ({e})"))
}

fn strategy_entry<'a>(stats: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    stats
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no stats for {name}: {stats:?}"))
}

// #780 — the file is now a directory: the shared engine/account
// fixtures stay here, and the tests are grouped by behaviour into
// child modules.
mod dispatch_boot_enablement_tests;
mod dispatch_cap_gate_tests;
mod dispatch_cycle_sizing_tests;
mod dispatch_policy_ledger_tests;
