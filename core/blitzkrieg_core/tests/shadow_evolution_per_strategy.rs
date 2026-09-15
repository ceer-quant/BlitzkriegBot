//! E2-c (#28) acceptance, end-to-end through the REAL `Core` + `Engine`:
//!
//!   1. TWO strategies evolve in parallel without cross-talk — one strategy's
//!      evolution moves only its own parameters, writes only its own audit file
//!      and leaves the other strategy's parameters, counters and history
//!      untouched. Isolation is structural: each strategy owns a distinct
//!      `ArcSwap` cell in the per-strategy registry, and its twin reads only
//!      that cell.
//!   2. Evolution OFF is the pre-change behaviour: no overlay is attached, no
//!      unit is registered, no file is created, and nothing acts on a parameter.
//!
//! The strategies are real, full `EngineStrategy` implementations with their own
//! declared knobs, their own twin factory and their own asset universe — the same
//! contract the builtin and an external dylib implement, so this exercises the
//! seam rather than a test double.

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::exit_policy::ExitConfig;
use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::scanner::ScannerConfig;
use blitzkrieg_core::service::{Core, CoreConfig, ShadowEvolutionTuning};
use blitzkrieg_core::shadow_evolution::{
    EvolutionOutcome, KnobSpec, MutableParams, ParamRegistry, StrategyParams,
};
use blitzkrieg_core::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use blitzkrieg_core::strategies::shadow_twin::ShadowFactory;
use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;

const T0: i64 = 1_700_000_000_000;
const ROUND_MS: i64 = 900_000;

// ── Two synthetic strategies, each with its OWN knob and asset ──────────────

/// Buys `asset` when the mid is at or under `cap`. One knob and one asset, so a
/// parameter change is provably the only source of a behavioural difference and
/// one strategy's token can be driven without moving the other's.
struct CapStrategy {
    name: String,
    asset: String,
    cap: Decimal,
    hot: Option<Arc<arc_swap::ArcSwap<StrategyParams>>>,
}

impl CapStrategy {
    fn new(name: &str, asset: &str, cap: Decimal) -> Self {
        Self {
            name: name.to_string(),
            asset: asset.to_string(),
            cap,
            hot: None,
        }
    }

    /// The cap in force: the declared value overlaid with this strategy's OWN
    /// cell. Reading only its own cell is what makes cross-talk impossible.
    fn effective_cap(&self) -> Decimal {
        match &self.hot {
            Some(h) => h.load().get("cap").unwrap_or(self.cap),
            None => self.cap,
        }
    }
}

impl EngineStrategy for CapStrategy {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64) {}

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        self.hot = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        vec![KnobSpec::new(
            "cap",
            self.effective_cap(),
            dec!(0.05),
            dec!(0.95),
        )]
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        Some(Box::new(CapFactory {
            name: self.name.clone(),
            asset: self.asset.clone(),
            cap: self.cap,
        }))
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let cap = self.effective_cap();
        let mut out = Vec::new();
        for market in ctx.markets().iter().filter(|m| m.asset == self.asset) {
            let token = market.up_token_id.clone();
            let Some(book) = ctx.fresh_book(&token) else {
                continue;
            };
            if book.mid_price > cap || book.mid_price <= Decimal::ZERO {
                continue;
            }
            out.push(TradeSignal {
                strategy: self.name.clone(),
                asset: market.asset.clone(),
                direction: SignalDirection::Up,
                token_id: token,
                condition_id: market.condition_id.clone(),
                price: book.best_ask.min(book.mid_price),
                reason: format!("mid {} <= cap {cap}", book.mid_price),
            });
        }
        out
    }
}

struct CapFactory {
    name: String,
    asset: String,
    cap: Decimal,
}

impl ShadowFactory for CapFactory {
    fn strategy(&self) -> String {
        self.name.clone()
    }
    fn knobs(&self) -> Vec<KnobSpec> {
        vec![KnobSpec::new("cap", self.cap, dec!(0.05), dec!(0.95))]
    }
    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        Some(Box::new(CapStrategy::new(
            &self.name,
            &self.asset,
            params.get("cap").unwrap_or(self.cap),
        )))
    }
}

/// Declares nothing → explicitly NOT evolvable.
struct Inert;

impl EngineStrategy for Inert {
    fn name(&self) -> &str {
        "inert"
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64) {}
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        Vec::new()
    }
}

// ── Harness ─────────────────────────────────────────────────────────────────

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "bk-e2c-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn engine_cfg() -> EngineConfig {
    EngineConfig {
        scanner: ScannerConfig {
            assets: vec!["BTC".into(), "ETH".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        // Short confirmation window. The builtin's own floor stays 0.55, which
        // the synthetic books never confirm, so it cannot quietly trade here.
        trend: TrendConfig {
            confirm_sec: 5,
            ratio: dec!(0.5),
            ..Default::default()
        },
        spread_arb: SpreadArbConfig::default(),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        ..Default::default()
    }
}

/// A core hosting the builtin `spread_arb` (disabled, so it cannot place orders)
/// plus the two synthetic strategies. The builtin declares knobs too, so it is
/// tracked as a third unit and must stay put throughout — a stricter isolation
/// check than two strategies alone.
fn build_core(dir: &std::path::Path, se_enabled: bool) -> Core {
    let cfg = CoreConfig {
        engine_enabled: true,
        dry_seed_balance: dec!(10_000),
        assets: vec!["BTC".into(), "ETH".into()],
        min_round_age_sec: 0,
        discovery_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        near_miss_path: None,
        shadow_evolution_enabled: se_enabled,
        shadow_evolution_tuning: Some(ShadowEvolutionTuning {
            min_sample_count: Some(2),
            min_win_rate_improvement: Some(dec!(0.05)),
            min_profit_factor_improvement: Some(dec!(0.10)),
            min_observation_secs: Some(0),
            cooldown_secs: Some(0),
            variant_count: Some(3),
            audit_dir: Some(dir.to_string_lossy().into_owned()),
        }),
        ..Default::default()
    };
    let mut core = Core::new(cfg);
    let mut eng = Engine::new(engine_cfg());
    // The builtin must not compete for the synthetic books.
    assert!(eng.set_strategy_enabled("spread_arb", false));
    eng.register_user_strategy(
        Box::new(CapStrategy::new("alpha", "BTC", dec!(0.40))),
        "test".into(),
    )
    .unwrap();
    eng.register_user_strategy(
        Box::new(CapStrategy::new("beta", "ETH", dec!(0.40))),
        "test".into(),
    )
    .unwrap();
    core.enable_engine(eng);
    if se_enabled {
        core.shadow_evolution_enable(T0);
    }
    core
}

fn market(asset: &str, up: &str, down: &str) -> CryptoMarket {
    CryptoMarket {
        asset: asset.into(),
        condition_id: format!("cond_{asset}"),
        question_id: format!("q_{asset}"),
        up_token_id: up.into(),
        down_token_id: down.into(),
        up_price: dec!(0.5),
        down_price: dec!(0.5),
        expires_at_ms: T0 + ROUND_MS,
        round_slot: T0 / ROUND_MS,
        neg_risk: true,
        question: format!("{asset} up or down"),
    }
}

fn feed_round(core: &mut Core, now: i64) {
    core.engine_on_data(
        DataEvent::RoundMarkets {
            markets: vec![
                market("BTC", "btc-up", "btc-down"),
                market("ETH", "eth-up", "eth-down"),
            ],
            now_ms: now,
        },
        now,
    );
}

fn feed_book(core: &mut Core, token: &str, bid: Decimal, ask: Decimal, now: i64) {
    core.engine_on_data(
        DataEvent::Book {
            token_id: token.into(),
            bids: vec![(bid, dec!(500))],
            asks: vec![(ask, dec!(500))],
            now_ms: now,
        },
        now,
    );
}

/// Drive ONE strategy's token through cycles in which its baseline LOSES and its
/// loosened-cap variant WINS. Over `rounds` cycles the loosened variant qualifies
/// on both win rate and profit factor while the baseline accumulates only losses.
///
/// The other strategy's asset is never fed here, so it cannot trade — which makes
/// "only this strategy moved" a real observation.
fn drive_cycles(core: &mut Core, token: &str, rounds: usize, t0: i64) -> i64 {
    let mut now = t0;
    for _ in 0..rounds {
        // A loser the baseline ALSO takes (mid 0.395 <= cap 0.40), then collapse
        // through the 12% stop.
        now += 1_000;
        feed_book(core, token, dec!(0.39), dec!(0.40), now);
        now += 1_000;
        feed_book(core, token, dec!(0.20), dec!(0.21), now);
        // A winner only a LOOSENED cap reaches (mid 0.405 > cap 0.40), then rally
        // through the 100% take-profit.
        now += 1_000;
        feed_book(core, token, dec!(0.40), dec!(0.41), now);
        now += 1_000;
        feed_book(core, token, dec!(0.95), dec!(0.97), now);
    }
    now
}

fn cap_of(core: &Core, strategy: &str) -> Option<Decimal> {
    core.shadow_evolution()
        .params_for(strategy)
        .and_then(|p| p.get("cap"))
}

// ── Acceptance 1: parallel evolution, no cross-talk ─────────────────────────

#[test]
fn two_strategies_evolve_in_parallel_without_cross_talk_through_the_core() {
    let dir = tmp_dir("parallel");
    std::fs::create_dir_all(&dir).unwrap();
    let mut core = build_core(&dir, true);

    // Every declaring strategy is registered — including the builtins, which are
    // hosted-but-disabled here (spread_arb on; trend_follow and the E4-b fade
    // leg off by default). That makes the isolation check stricter: five units,
    // one moves.
    assert_eq!(
        core.shadow_evolution().strategy_names(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string(),
            "alpha".to_string(),
            "beta".to_string(),
        ],
    );
    let reg = core.shadow_evolution().registry();
    let c_spread = reg
        .handle_for("spread_arb")
        .expect("the builtin declares knobs");
    let c_alpha = reg.handle_for("alpha").expect("alpha declared a knob");
    let c_beta = reg.handle_for("beta").expect("beta declared a knob");
    assert!(
        !Arc::ptr_eq(&c_alpha, &c_beta),
        "alpha and beta own distinct cells"
    );
    assert!(
        !Arc::ptr_eq(&c_alpha, &c_spread) && !Arc::ptr_eq(&c_beta, &c_spread),
        "no strategy shares the builtin's cell",
    );

    feed_round(&mut core, T0);

    // ── Phase 1: only BTC moves ⇒ only alpha may evolve ────────────────────
    let end = drive_cycles(&mut core, "btc-up", 3, T0 + 1_000);

    let alpha_before = cap_of(&core, "alpha").unwrap();
    let beta_before = cap_of(&core, "beta").unwrap();
    let spread_before = core.shadow_evolution().params_for("spread_arb").unwrap();

    core.engine_evaluate(end + 100_000);

    assert_ne!(
        cap_of(&core, "alpha").unwrap(),
        alpha_before,
        "alpha must have evolved"
    );
    assert_eq!(
        cap_of(&core, "beta"),
        Some(beta_before),
        "beta's parameters must not move"
    );
    assert_eq!(
        core.shadow_evolution().params_for("spread_arb"),
        Some(spread_before),
        "the builtin's parameters must not move",
    );
    assert_eq!(core.shadow_evolution().evolution_count("alpha"), 1);
    assert_eq!(core.shadow_evolution().evolution_count("beta"), 0);
    assert_eq!(core.shadow_evolution().evolution_count("spread_arb"), 0);

    // Audit: alpha's own file exists and names only alpha. No write may create
    // another strategy's file.
    let alpha_log = dir.join("alpha.jsonl");
    assert!(alpha_log.exists(), "alpha's own audit file");
    assert!(
        !dir.join("beta.jsonl").exists(),
        "beta never evolved, so it has no file"
    );
    assert!(!dir.join("spread_arb.jsonl").exists());
    let text = std::fs::read_to_string(&alpha_log).unwrap();
    assert!(text.contains("\"strategy\":\"alpha\""), "{text}");
    assert!(
        !text.contains("beta"),
        "alpha's file must not mention beta: {text}"
    );
    assert!(
        !text.contains("spread_arb"),
        "alpha's file must not mention the builtin: {text}"
    );
    assert!(core.shadow_evolution_history(Some("beta"), 10).is_empty());
    assert!(!core.shadow_evolution_history(Some("alpha"), 10).is_empty());
    assert_eq!(
        core.shadow_evolution().audited_strategies(),
        vec!["alpha".to_string()],
        "only alpha has history",
    );

    // ── Phase 2: only ETH moves ⇒ only beta may evolve ─────────────────────
    let alpha_frozen = cap_of(&core, "alpha").unwrap();
    let end2 = drive_cycles(&mut core, "eth-up", 3, end + 200_000);
    core.engine_evaluate(end2 + 100_000);

    assert_eq!(
        cap_of(&core, "alpha"),
        Some(alpha_frozen),
        "alpha must not move on another strategy's tokens",
    );
    assert_ne!(
        cap_of(&core, "beta").unwrap(),
        beta_before,
        "beta must have evolved now"
    );
    assert_eq!(core.shadow_evolution().evolution_count("alpha"), 1);
    assert_eq!(core.shadow_evolution().evolution_count("beta"), 1);
    assert_eq!(core.shadow_evolution().evolution_count("spread_arb"), 0);

    let beta_log = dir.join("beta.jsonl");
    let beta_text = std::fs::read_to_string(&beta_log).unwrap();
    assert!(beta_text.contains("\"strategy\":\"beta\""), "{beta_text}");
    assert!(
        !beta_text.contains("alpha"),
        "beta's file must not mention alpha: {beta_text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Acceptance 2: evolution OFF is the pre-change behaviour ─────────────────

#[test]
fn evolution_off_is_byte_for_byte_the_previous_behaviour() {
    let dir = tmp_dir("off");
    std::fs::create_dir_all(&dir).unwrap();
    let mut core = build_core(&dir, false);

    // Nothing registered, no overlay attached, nothing to evaluate.
    assert!(!core.shadow_evolution().is_enabled());
    assert!(core.shadow_evolution().strategy_names().is_empty());
    assert!(
        !core.has_hot_params(),
        "the overlay must be detached, not merely ignored"
    );
    assert_eq!(core.shadow_evolution().variant_count(), 0);

    feed_round(&mut core, T0);
    let end = drive_cycles(&mut core, "btc-up", 3, T0 + 1_000);
    drive_cycles(&mut core, "eth-up", 3, end + 200_000);
    core.engine_evaluate(end + 500_000);

    // No evolution ran, no parameters were invented, no audit file was created:
    // both strategies ran on exactly the config the host pushed.
    assert!(core.shadow_evolution().strategy_names().is_empty());
    assert!(core.shadow_evolution().current_params().is_empty());
    assert_eq!(core.shadow_evolution().variant_count(), 0);
    assert!(core.shadow_evolution_history(None, 100).is_empty());
    assert!(!core.has_hot_params());
    assert!(
        !dir.join("alpha.jsonl").exists(),
        "an inert engine must create no file"
    );
    assert!(!dir.join("beta.jsonl").exists());
    assert!(!dir.join("spread_arb.jsonl").exists());

    // Enabling the SAME core afterwards registers the declarations at their
    // in-force values — proving the off path was not secretly maintaining state.
    core.shadow_evolution_enable(end + 600_000);
    assert_eq!(
        cap_of(&core, "alpha"),
        Some(dec!(0.40)),
        "the live value, not an invented one"
    );
    assert_eq!(cap_of(&core, "beta"), Some(dec!(0.40)));
    assert!(core.has_hot_params(), "enabling attaches the overlay");
    // Attaching is a no-op overlay: it does not itself move a value.
    assert_eq!(cap_of(&core, "alpha"), Some(dec!(0.40)));

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Acceptance 3: per-strategy apply/rollback through the Core surface ──────

#[test]
fn apply_and_rollback_move_exactly_one_strategy() {
    let dir = tmp_dir("apply");
    std::fs::create_dir_all(&dir).unwrap();
    let mut core = build_core(&dir, true);

    let bag = |strategy: &str, cap: Decimal| {
        let mut p = StrategyParams::new();
        p.set("cap", cap);
        let mut m = MutableParams::new();
        m.set_strategy(strategy, p);
        m
    };

    // +3%, inside the ±5% gradient lock.
    core.shadow_evolution_apply(bag("alpha", dec!(0.412)), 1_000)
        .unwrap();
    assert_eq!(cap_of(&core, "alpha"), Some(dec!(0.412)));
    assert_eq!(cap_of(&core, "beta"), Some(dec!(0.40)), "beta untouched");

    match core.shadow_evolution_rollback("alpha", 2_000).unwrap() {
        EvolutionOutcome::RolledBack { strategy, from, to } => {
            assert_eq!(strategy, "alpha");
            assert_eq!(from.get("alpha", "cap"), Some(dec!(0.412)));
            assert_eq!(to.get("alpha", "cap"), Some(dec!(0.40)));
        }
        _ => panic!("expected a rollback outcome"),
    }
    assert_eq!(cap_of(&core, "alpha"), Some(dec!(0.40)));

    // A strategy that never changed anything has nothing to roll back to — an
    // explicit error, not a silent no-op another strategy's rollback could be
    // mistaken for.
    assert!(core.shadow_evolution_rollback("beta", 3_000).is_err());
    assert!(
        core.shadow_evolution_rollback("not_a_strategy", 4_000)
            .is_err()
    );

    // A multi-strategy bag is refused: apply is per-strategy, so nothing is
    // partially applied.
    let mut both = bag("alpha", dec!(0.41));
    both.set_strategy("beta", StrategyParams::new());
    assert!(core.shadow_evolution_apply(both, 5_000).is_err());
    assert_eq!(
        cap_of(&core, "alpha"),
        Some(dec!(0.40)),
        "still the rolled-back value"
    );

    // An out-of-domain proposal is refused before it can reach the strategy.
    assert!(
        core.shadow_evolution_apply(bag("alpha", dec!(0.99)), 6_000)
            .is_err()
    );
    assert_eq!(cap_of(&core, "alpha"), Some(dec!(0.40)));

    // Manual changes are audited, and only in the strategy's own file.
    let recs = core.shadow_evolution_history(Some("alpha"), 10);
    assert_eq!(recs.len(), 2, "apply + rollback: {recs:?}");
    assert!(recs.iter().all(|r| r.strategy == "alpha"));
    assert!(
        recs.iter().all(|r| r.manual),
        "an operator override must leave a trace"
    );
    let text = std::fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
    assert!(!text.contains("\"strategy\":\"beta\""));
    assert!(!dir.join("beta.jsonl").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Acceptance 4: an undeclared strategy is explicitly not evolvable ────────

#[test]
fn an_undeclared_strategy_is_reported_not_evolvable() {
    let dir = tmp_dir("inert");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = CoreConfig {
        engine_enabled: true,
        dry_seed_balance: dec!(1000),
        discovery_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        near_miss_path: None,
        shadow_evolution_enabled: true,
        shadow_evolution_tuning: Some(ShadowEvolutionTuning {
            audit_dir: Some(dir.to_string_lossy().into_owned()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut core = Core::new(cfg);
    let mut eng = Engine::new(engine_cfg());
    eng.register_user_strategy(Box::new(Inert), "test".into())
        .unwrap();
    core.enable_engine(eng);
    core.shadow_evolution_enable(0);

    let ev = core.shadow_evolution();
    assert!(ev.declared_knobs("inert").is_empty());
    assert!(
        ev.params_for("inert").is_none(),
        "not evolvable is an explicit answer"
    );
    assert!(ev.status("inert", 0).is_none());
    // The builtin DOES declare knobs, so it is evolvable — the seam is about the
    // declaration, not about a strategy being synthetic or builtin.
    assert!(!ev.declared_knobs("spread_arb").is_empty());
    assert!(ev.params_for("spread_arb").is_some());
    // Its knobs carry coherent domains — what the evaluator and the gradient
    // lock check a proposal against.
    for k in ev.declared_knobs("spread_arb") {
        assert!(k.is_coherent(), "{} declares an incoherent domain", k.name);
    }

    // The exit policy the twins replay is the LIVE position manager's policy
    // (D-2), not a separate default that would judge variants against an exit
    // mechanism the live path never runs.
    let live: ExitConfig = core.config().positions.exit.clone();
    assert_eq!(live.stop_loss_pct, ExitConfig::default().stop_loss_pct);

    let _ = std::fs::remove_dir_all(&dir);
}
