use super::*;
use crate::exit_policy::ExitConfig;
use crate::model::SignalDirection;
use crate::signal::TradeSignal;
use crate::strategies::StrategyCtx;
use crate::strategies::shadow_twin::{ShadowFactory, tick_ctx};
use crate::test_fixtures::{evo_book, evo_market};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A synthetic strategy: buys when the mid is at or under `cap`. Exactly one
/// knob, so a parameter change is provably the only source of a difference.
struct CapStrategy {
    name: String,
    cap: Decimal,
}

struct CapFactory {
    name: String,
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
        Some(Box::new(CapStrategy {
            name: self.name.clone(),
            cap: params.get("cap").unwrap_or(self.cap),
        }))
    }
}

impl EngineStrategy for CapStrategy {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        vec![KnobSpec::new("cap", self.cap, dec!(0.05), dec!(0.95))]
    }
    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        Some(Box::new(CapFactory {
            name: self.name.clone(),
            cap: self.cap,
        }))
    }
    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let market = &ctx.markets()[0];
        let token = market.up_token_id.clone();
        let Some(book) = ctx.fresh_book(&token) else {
            return Vec::new();
        };
        if book.mid_price > self.cap {
            return Vec::new();
        }
        vec![TradeSignal {
            strategy: self.name.clone(),
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            asset: market.asset.clone(),
            direction: SignalDirection::Up,
            token_id: token,
            condition_id: market.condition_id.clone(),
            price: book.mid_price,
            reason: format!("mid {} <= cap {}", book.mid_price, self.cap),
            shares: None,
            pair: None,
        }]
    }
}

/// Declares nothing — explicitly not evolvable.
struct Inert;

impl EngineStrategy for Inert {
    fn name(&self) -> &str {
        "inert"
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        Vec::new()
    }
}

fn fast_cfg(enabled: bool, tag: &str) -> ShadowEvolutionConfig {
    ShadowEvolutionConfig {
        enabled,
        min_sample_count: 2,
        min_win_rate_improvement: dec!(0.05),
        min_profit_factor_improvement: dec!(0.10),
        min_observation_secs: 0,
        cooldown_secs: 0,
        variant_count: 3,
        audit_dir: std::env::temp_dir()
            .join(format!("bkevo-{}-{tag}", std::process::id()))
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    }
}

fn strategies() -> (CapStrategy, CapStrategy) {
    (
        CapStrategy {
            name: "alpha".into(),
            cap: dec!(0.40),
        },
        CapStrategy {
            name: "beta".into(),
            cap: dec!(0.40),
        },
    )
}

/// Drive ONE strategy's twin set through `times` cycles where the baseline
/// LOSES and a variant WINS: a dip the baseline's own cap admits (mid
/// 0.39 <= 0.40) that collapses, then — one tick later — a collapsed price
/// (mid 0.20) a variant that MISSED the dip is free to buy and ride all the
/// way back up. The baseline is already holding through the crash, so it
/// stops out at the collapse and keeps samples but a zero win rate, while
/// the crash-entry variant beats it on win rate and profit factor.
///
/// The dip and crash books are LOCKED (bid = ask = mid): under the F7
/// fillability rule a resting bid only fills when the offer side reaches
/// its price, so the offer must sit at the entry level for a position to
/// open. The rally book's offer deliberately sits ABOVE every entry limit —
/// a signal to re-enter the rally is therefore not fillable and produces no
/// trade (F7), keeping the comparison down to executable trades only.
fn drive_wins(m: &mut ShadowEvolution, name: &str, times: usize, t0: i64) {
    let round = evo_market();
    let i = m.units.iter().position(|u| u.strategy == name).unwrap();
    let mut now = t0;
    for _ in 0..times {
        // A loser the baseline also takes: entry under the cap, then collapse.
        now += 1_000;
        let dip = evo_book(0.39, 0.39); // mid 0.39 — locked, offer at the entry price
        for v in m.units[i].set.variants.iter_mut() {
            v.on_tick(&tick_ctx(
                std::slice::from_ref(&round),
                "t",
                &dip,
                1,
                880,
                now,
            ));
        }
        now += 1_000;
        // LOCKED at the crash level: holders stop out at the 0.20 bid, and a
        // variant that missed the dip may take the collapsed price — its
        // offer must reach the entry level for that entry to be fillable.
        let crash = evo_book(0.20, 0.20); // mid 0.20 → well past the 12% stop
        for v in m.units[i].set.variants.iter_mut() {
            v.on_tick(&tick_ctx(
                std::slice::from_ref(&round),
                "t",
                &crash,
                1,
                880,
                now,
            ));
        }
        // The rally: mid 0.41 clears the 0.40 baseline cap, but its OFFER
        // (0.43) sits above every entry limit, so re-entering here is not
        // fillable (F7) — and its bid stays under the 100% take-profit
        // floor, so the collapsed-price entry rides on to the up tick.
        now += 1_000;
        let shallow = evo_book(0.39, 0.43); // mid 0.41 > the 0.40 baseline cap
        for v in m.units[i].set.variants.iter_mut() {
            v.on_tick(&tick_ctx(
                std::slice::from_ref(&round),
                "t",
                &shallow,
                1,
                880,
                now,
            ));
        }
        now += 1_000;
        let up = evo_book(0.95, 0.97);
        for v in m.units[i].set.variants.iter_mut() {
            v.on_tick(&tick_ctx(
                std::slice::from_ref(&round),
                "t",
                &up,
                1,
                880,
                now,
            ));
        }
    }
}

#[test]
fn disabled_by_default_is_inert() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut m = ShadowEvolution::new(fast_cfg(false, "off"), &refs);
    assert!(!m.is_enabled());
    assert_eq!(m.aggregate_status(0), EvolutionStatus::Disabled);
    m.on_round(&[evo_market()], &[], 1000);
    m.on_tick("t", &evo_book(0.43, 0.45), 1000);
    assert_eq!(m.variant_count(), 0);
    assert!(m.evaluate(1000).is_empty());
    assert!(m.history(None, 10).is_empty());
    // The declared values are still published, so attaching the registry is a
    // no-op overlay rather than an invented parameter.
    assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.40)));
}

#[test]
fn a_strategy_that_declares_nothing_gets_no_unit() {
    let (a, _b) = strategies();
    let inert = Inert;
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &inert];
    let m = ShadowEvolution::new(fast_cfg(false, "inert"), &refs);
    assert_eq!(m.strategy_names(), vec!["alpha".to_string()]);
    assert!(m.params_for("inert").is_none(), "not evolvable is explicit");
    assert!(m.declared_knobs("inert").is_empty());
    assert!(m.status("inert", 0).is_none());
}

/// Issue #481: a re-registration must keep the value IN FORCE, not stomp it
/// back to the declared default. The doc on `register_strategies` promises
/// "keeps state"; for the knobs the in-tree adapters derive from their
/// effective config that promise holds trivially (the declaration already
/// carries the value), but a Lua package's declaration is the static
/// manifest default — an operator's `strategy.params.set` write would be
/// wiped by any rewire (a sibling strategy toggling, a package loaded
/// later, the engine being installed). This is the shape that exposes it:
/// static declarations, value moved by hand, then re-register.
#[test]
fn re_registration_keeps_the_value_in_force_within_its_domain() {
    let (mut a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let mut m = ShadowEvolution::new(fast_cfg(false, "keep-value"), &refs);
    assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.40)));

    // The operator moves the knob (same shape strategy.params.set lands).
    let mut bag = m.params_for("alpha").unwrap();
    bag.set("cap", dec!(0.60));
    m.registry().publish("alpha", bag);
    assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.60)));

    // A rewire hands a FRESH instance of the strategy to the manager (a
    // toggle or a later package load builds new adapters; the declaration
    // is the static manifest default again).
    a.cap = dec!(0.40);
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    m.register_strategies(&refs, 2000);

    assert_eq!(
        m.registry().get("alpha", "cap"),
        Some(dec!(0.60)),
        "re-registering a strategy must keep the value in force, not the declared default"
    );
}

/// The carry-over is bounded by the declaration: a value in force that the
/// NEW declaration no longer admits falls back to the declared default —
/// the declared domain is the hard boundary on the set path, and it stays
/// one on the re-registration path. Narrower manifest = the package
/// reloads having tightened its own tunables domain.
#[test]
fn re_registration_falls_back_when_the_value_left_the_declared_domain() {
    struct NarrowCap {
        name: String,
    }
    impl EngineStrategy for NarrowCap {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
        fn evolvable_knobs(&self) -> Vec<KnobSpec> {
            // Same knob, but the domain no longer admits 0.60.
            vec![KnobSpec::new("cap", dec!(0.40), dec!(0.05), dec!(0.50))]
        }
        fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
            Some(Box::new(CapFactory {
                name: self.name.clone(),
                cap: dec!(0.40),
            }))
        }
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            Vec::new()
        }
    }
    let (mut a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let mut m = ShadowEvolution::new(fast_cfg(false, "keep-value-domain"), &refs);

    let mut bag = m.params_for("alpha").unwrap();
    bag.set("cap", dec!(0.60));
    m.registry().publish("alpha", bag);
    assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.60)));

    // The package reloads with a narrower manifest domain that no longer
    // admits the value in force.
    a.cap = dec!(0.40);
    let narrow = NarrowCap {
        name: "alpha".to_string(),
    };
    let refs: Vec<&dyn EngineStrategy> = vec![&narrow];
    m.register_strategies(&refs, 2000);

    assert_eq!(
        m.registry().get("alpha", "cap"),
        Some(dec!(0.40)),
        "a value outside the new declaration must fall back to the declared default"
    );
}

#[test]
fn enable_scaffolds_every_strategy_and_publishes_its_own_cell() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut m = ShadowEvolution::new(fast_cfg(false, "enable"), &refs);
    m.enable(0);
    assert_eq!(m.variant_count(), 6, "3 variants x 2 strategies");
    let ca = m.registry().handle_for("alpha").unwrap();
    let cb = m.registry().handle_for("beta").unwrap();
    assert_ne!(
        Arc::as_ptr(&ca),
        Arc::as_ptr(&cb),
        "each strategy has its own cell"
    );
    let views = m.variant_views(0);
    assert!(views.iter().any(|v| v.strategy == "alpha" && v.is_baseline));
    assert!(views.iter().any(|v| v.strategy == "beta" && v.is_baseline));
}

/// ACCEPTANCE (issue #28): two strategies evolving in parallel must not
/// cross-talk. Only alpha's variants qualify ⇒ only alpha's parameters move,
/// only alpha's audit file gains an applied record, beta is unchanged. Then
/// the reverse.
#[test]
fn two_strategies_evolve_in_parallel_without_cross_talk() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    // The pre-proposal (unattended) semantics: a qualifying variant applies
    // directly, which is what this acceptance was written against.
    let mut cfg = fast_cfg(false, "parallel");
    cfg.auto_evolve = true;
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);

    // Only alpha produces qualifying variants.
    drive_wins(&mut m, "alpha", 2, 10_000);

    let beta_before = m.registry().get("beta", "cap").unwrap();
    let outcomes = m.evaluate(100_000);
    assert_eq!(outcomes.len(), 1, "exactly one strategy evolved");
    match &outcomes[0] {
        EvolutionOutcome::Applied(sig) => {
            assert_eq!(sig.strategy, "alpha");
            assert_eq!(sig.to_params.strategies(), vec!["alpha"]);
        }
        EvolutionOutcome::Rejected { signal, reason } => {
            panic!(
                "alpha should have applied, was rejected: {} ({reason})",
                signal.strategy
            )
        }
        _ => panic!("expected an applied outcome"),
    }

    assert_ne!(m.registry().get("alpha", "cap").unwrap(), dec!(0.40));
    assert_eq!(m.registry().get("beta", "cap").unwrap(), beta_before);
    assert_eq!(m.evolution_count("alpha"), 1);
    assert_eq!(m.evolution_count("beta"), 0);
    assert_eq!(m.audited_strategies(), vec!["alpha".to_string()]);
    assert!(
        m.history(Some("beta"), 10).is_empty(),
        "beta has no records"
    );
    assert_eq!(m.history(Some("alpha"), 10).len(), 1);
    assert!(dir.join("alpha.jsonl").exists());
    assert!(
        !dir.join("beta.jsonl").exists(),
        "no write may create beta's file"
    );
    let alpha_log = std::fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
    assert!(alpha_log.contains("\"strategy\":\"alpha\""));
    assert!(!alpha_log.contains("beta"));

    // Reverse: beta wins, alpha must not move again.
    let alpha_after = m.registry().get("alpha", "cap").unwrap();
    drive_wins(&mut m, "beta", 2, 200_000);
    let outcomes = m.evaluate(300_000);
    assert_eq!(outcomes.len(), 1, "only beta should propose");
    match &outcomes[0] {
        EvolutionOutcome::Applied(sig) => assert_eq!(sig.strategy, "beta"),
        EvolutionOutcome::Rejected { signal, reason } => {
            panic!(
                "beta should have applied, was rejected: {} ({reason})",
                signal.strategy
            )
        }
        _ => panic!("expected an applied outcome"),
    }
    assert_eq!(
        m.registry().get("alpha", "cap").unwrap(),
        alpha_after,
        "alpha must not move"
    );
    assert_eq!(m.evolution_count("alpha"), 1);
    assert_eq!(m.evolution_count("beta"), 1);
    let beta_log = std::fs::read_to_string(dir.join("beta.jsonl")).unwrap();
    assert!(beta_log.contains("\"strategy\":\"beta\""));
    assert!(
        !beta_log.contains("alpha"),
        "beta's file must not mention alpha"
    );
    // And the two proposals differ: each carries its own knob set.
    assert!(
        m.history(Some("alpha"), 10)
            .iter()
            .all(|r| r.strategy == "alpha")
    );
    assert!(
        m.history(Some("beta"), 10)
            .iter()
            .all(|r| r.strategy == "beta")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rollback_is_per_strategy() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut m = ShadowEvolution::new(fast_cfg(false, "rollback"), &refs);
    let dir = std::path::PathBuf::from(&m.cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    m.enable(0);
    let original = m.registry().get("alpha", "cap").unwrap();

    let mut p = StrategyParams::new();
    p.set("cap", dec!(0.412));
    m.set_params("alpha", p, 1000).unwrap();
    assert_eq!(m.registry().get("alpha", "cap").unwrap(), dec!(0.412));

    match m.rollback("alpha", 2000).unwrap() {
        EvolutionOutcome::RolledBack { strategy, from, to } => {
            assert_eq!(strategy, "alpha");
            assert_eq!(from.get("alpha", "cap"), Some(dec!(0.412)));
            assert_eq!(to.get("alpha", "cap"), Some(original));
        }
        _ => panic!("expected a rollback"),
    }
    assert_eq!(m.registry().get("alpha", "cap").unwrap(), original);
    // beta never changed, so its rollback is an explicit error, not a silent
    // no-op that could be mistaken for alpha's.
    assert!(m.rollback("beta", 3000).is_err());
    assert!(m.rollback("nope", 3000).is_err());
    assert_eq!(
        m.history(Some("beta"), 10).len(),
        0,
        "beta's history stays empty"
    );
    assert_eq!(
        m.history(Some("alpha"), 10).len(),
        1,
        "only alpha's rollback was recorded"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_manual_apply_is_audited_and_domain_checked() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut m = ShadowEvolution::new(fast_cfg(false, "manual"), &refs);
    let dir = std::path::PathBuf::from(&m.cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    m.enable(0);

    let mut params = MutableParams::new();
    let mut p = StrategyParams::new();
    p.set("cap", dec!(0.412)); // +3%, inside the gradient lock
    params.set_strategy("alpha", p);
    m.apply_params(params, 5000).unwrap();
    let rec = m.history(Some("alpha"), 10);
    assert_eq!(rec.len(), 1);
    assert!(
        rec[0].manual && rec[0].applied,
        "a manual override is traced now"
    );
    assert_eq!(rec[0].from_params.get("alpha", "cap"), Some(dec!(0.40)));
    assert_eq!(rec[0].to_params.get("alpha", "cap"), Some(dec!(0.412)));

    // Out of domain → rejected, and the value does not move.
    let before = m.registry().get("alpha", "cap").unwrap();
    let mut p = StrategyParams::new();
    p.set("cap", dec!(0.99)); // beyond the declared 0.95 ceiling
    let mut params = MutableParams::new();
    params.set_strategy("alpha", p);
    assert!(m.apply_params(params, 6000).is_err());
    assert_eq!(m.registry().get("alpha", "cap").unwrap(), before);

    // An undeclared knob is rejected, not silently dropped.
    let mut p = StrategyParams::new();
    p.set("hard_stop_loss_pct", dec!(0));
    let mut params = MutableParams::new();
    params.set_strategy("alpha", p);
    assert!(m.apply_params(params, 7000).is_err());

    // More than one strategy at a time is refused: apply is per-strategy.
    let mut params = MutableParams::new();
    let mut p = StrategyParams::new();
    p.set("cap", dec!(0.40));
    params.set_strategy("alpha", p.clone());
    params.set_strategy("beta", p);
    assert!(m.apply_params(params, 8000).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn observation_alone_never_moves_parameters() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut m = ShadowEvolution::new(fast_cfg(false, "observe"), &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    m.on_tick("t", &evo_book(0.43, 0.45), 1000);
    m.on_tick("t", &evo_book(0.95, 0.97), 2000);
    let views = m.variant_views(2000);
    assert!(views.iter().any(|v| v.is_baseline));
    assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.40)));
    assert_eq!(m.registry().get("beta", "cap"), Some(dec!(0.40)));
}

#[test]
fn guard_rejects_gradient_beyond_lock() {
    let mut base = StrategyParams::new();
    base.set("cap", dec!(0.40));
    let mut far = StrategyParams::new();
    far.set("cap", dec!(0.48)); // +20%
    assert!(guard::validate_gradient(&base, &far, dec!(0.05)).is_err());
    assert!(guard::validate_gradient(&base, &base.scaled(dec!(1.04)), dec!(0.05)).is_ok());
}

#[test]
fn the_exit_policy_replayed_is_the_configured_one() {
    // D-2: variants must replay the SAME exit config the live position manager
    // uses. The manager forwards `cfg.exit_cfg` verbatim into every twin.
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut cfg = fast_cfg(false, "exit");
    cfg.exit_cfg = ExitConfig {
        stop_loss_pct: dec!(12),
        ..ExitConfig::default()
    };
    let m = ShadowEvolution::new(cfg, &refs);
    assert_eq!(m.cfg.exit_cfg.stop_loss_pct, dec!(12));
}

/// A twin whose strategy code panics on every decision — the archetype of a
/// buggy user-authored variant (E7 lets users ship these as dylibs).
struct PanickingTwin {
    name: String,
}

impl EngineStrategy for PanickingTwin {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        panic!("user-authored twin exploded");
    }
}

struct PanicFactory {
    name: String,
}

impl ShadowFactory for PanicFactory {
    fn strategy(&self) -> String {
        self.name.clone()
    }
    fn knobs(&self) -> Vec<KnobSpec> {
        vec![KnobSpec::new("cap", dec!(0.40), dec!(0.05), dec!(0.95))]
    }
    fn make(&self, _params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        Some(Box::new(PanickingTwin {
            name: self.name.clone(),
        }))
    }
}

struct PanicStrategy {
    name: String,
}

impl EngineStrategy for PanicStrategy {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        vec![KnobSpec::new("cap", dec!(0.40), dec!(0.05), dec!(0.95))]
    }
    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        Some(Box::new(PanicFactory {
            name: self.name.clone(),
        }))
    }
    fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        Vec::new()
    }
}

/// E10-d's `panic = "abort"` decision (D-20) rests on this guarantee, so it
/// must be pinned rather than assumed: a variant that panics on every tick
/// must NOT unwind into the caller of `on_tick`, and the healthy variants
/// alongside it must keep receiving ticks.
///
/// This is load-bearing precisely because variants are user-authored dylibs.
#[test]
fn a_panicking_variant_cannot_take_down_the_engine() {
    let boom = PanicStrategy {
        name: "boom".into(),
    };
    let (good, _) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&boom, &good];
    let mut m = ShadowEvolution::new(fast_cfg(true, "panic"), &refs);
    m.on_round(&[evo_market()], &[], 1_000);

    // A tick that makes every twin want to enter (mid 0.395 under any cap).
    // If the panic escaped, this call itself would abort the test.
    m.on_tick("t", &evo_book(0.39, 0.40), 1_000);
    // And a second tick, so a "quarantine" that only works once is caught.
    m.on_tick("t", &evo_book(0.39, 0.40), 2_000);

    // The healthy strategy's twins must still have been fed: the panic is
    // isolated, not a reason to skip the rest of the loop.
    let good_unit = m
        .units
        .iter()
        .find(|u| u.strategy == "alpha")
        .expect("the healthy strategy keeps its unit");
    assert!(
        good_unit.set.variants.iter().all(|v| !v.crashed),
        "the healthy variant must not be collateral damage"
    );

    // The evaluation pass must also survive reading a twin that panicked.
    let _ = m.evaluate(3_000);
}

/// KI-23: the twin's own panic is ABSORBED (nothing propagates), but it must
/// LATCH `crashed` — a permanently panicking variant is quarantined on its
/// first panicking tick instead of being re-ticked (and unwinding) forever,
/// silently. The panic still does not take the engine down, and the second
/// tick must skip the quarantined variant entirely (no further unwind).
#[test]
fn a_permanently_panicking_twin_is_quarantined_on_its_first_panic() {
    let boom = PanicStrategy {
        name: "boom".into(),
    };
    let refs: Vec<&dyn EngineStrategy> = vec![&boom];
    let mut m = ShadowEvolution::new(fast_cfg(true, "quarantine"), &refs);
    m.on_round(&[evo_market()], &[], 1_000);

    // First tick: every variant of "boom" panics inside its own code — the
    // panic is absorbed (this call must not unwind) and `crashed` latches.
    m.on_tick("t", &evo_book(0.39, 0.40), 1_000);
    assert!(
        m.units[0].set.variants.iter().all(|v| v.crashed),
        "a permanently panicking twin must latch `crashed` on its first panic"
    );

    // Second tick: the quarantined variants are skipped — if the flag were
    // not consulted, this would unwind again (and the absorb would hide it,
    // but the skip is what stops the per-tick unwind cost).
    m.on_tick("t", &evo_book(0.39, 0.40), 2_000);

    // The evaluation pass must also survive reading a twin that panicked.
    let _ = m.evaluate(3_000);
}

// ── E13 (#95): the proposal workflow ─────────────────────────────────────

/// MANUAL mode: a qualifying signal does NOT move the cell — it becomes a
/// pending proposal the operator decides over IPC, and the twin comparison
/// block carries the same figures the auto path was judged on.
#[test]
fn manual_mode_holds_a_proposal_instead_of_applying() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let mut cfg = fast_cfg(false, "hold");
    cfg.auto_evolve = false;
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);

    let cap_before = m.registry().get("alpha", "cap").unwrap();
    let outcomes = m.evaluate(100_000);
    assert_eq!(outcomes.len(), 1);
    let proposal = match &outcomes[0] {
        EvolutionOutcome::Proposed(p) => p,
        other => panic!("expected a held proposal, got {other:?}"),
    };
    assert_eq!(proposal.strategy, "alpha");
    assert_eq!(proposal.state, ProposalState::Proposed);
    assert_eq!(proposal.baseline.closed, 2, "comparison block populated");
    assert_eq!(proposal.variant.closed, 2);
    assert!(
        proposal.variant.win_rate > proposal.baseline.win_rate,
        "the comparison must show WHY the variant qualifies"
    );
    assert!(
        proposal.expires_at_ms > 100_000,
        "the TTL is set from creation"
    );
    // The cell did not move, and the proposal is on disk.
    assert_eq!(m.registry().get("alpha", "cap").unwrap(), cap_before);
    assert_eq!(m.pending_proposal_count(), 1);

    // An identical re-proposal is a no-op (the operator still has exactly
    // one pending decision for this strategy).
    drive_wins(&mut m, "alpha", 2, 200_000);
    let outcomes2 = m.evaluate(300_000);
    let proposed_again: Vec<_> = outcomes2
        .iter()
        .filter(|o| matches!(o, EvolutionOutcome::Proposed(_)))
        .collect();
    assert!(
        proposed_again.is_empty(),
        "an identical re-proposal is a no-op"
    );
    assert_eq!(m.pending_proposal_count(), 1);
}

/// REJECT closes the proposal without touching anything; the same variant
/// may not re-propose over a decided one silently (the twin keeps its own
/// cooldown via the shared re-anchor).
#[test]
fn rejecting_a_proposal_closes_it_and_moves_nothing() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "reject");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let outcomes = m.evaluate(100_000);
    let proposal = match &outcomes[0] {
        EvolutionOutcome::Proposed(p) => p.clone(),
        other => panic!("expected a held proposal, got {other:?}"),
    };

    let cap_before = m.registry().get("alpha", "cap").unwrap();
    let result = m
        .decide(&proposal.id, Decision::Reject, 200_000)
        .expect("rejection is a legal decision");
    match result {
        DecisionResult::Rejected { proposal: p } => {
            assert_eq!(p.state, ProposalState::Rejected);
            assert_eq!(p.decided_by, Some(DecidedBy::User));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert_eq!(m.registry().get("alpha", "cap").unwrap(), cap_before);
    assert_eq!(m.pending_proposal_count(), 0);
    // A decided proposal cannot be re-decided.
    assert!(
        m.decide(&proposal.id, Decision::Accept, 300_000).is_err(),
        "a decided proposal is closed"
    );
}

/// ACCEPT runs the FULL guard chain against the parameters in force at
/// decision time, then hot-swaps, re-anchors, audits and logs a promotion.
#[test]
fn accepting_a_proposal_hot_swaps_and_logs_a_durable_promotion() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "accept");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let outcomes = m.evaluate(100_000);
    let proposal = match &outcomes[0] {
        EvolutionOutcome::Proposed(p) => p.clone(),
        other => panic!("expected a held proposal, got {other:?}"),
    };

    let result = m
        .decide(&proposal.id, Decision::Accept, 200_000)
        .expect("acceptance must pass the guards");
    let accepted = match result {
        DecisionResult::Accepted { proposal: p } => p,
        other => panic!("expected acceptance, got {other:?}"),
    };
    assert_eq!(accepted.state, ProposalState::Accepted);
    assert_ne!(
        m.registry().get("alpha", "cap").unwrap(),
        dec!(0.40),
        "the cell moved"
    );
    assert_eq!(m.evolution_count("alpha"), 1);

    // The promotion is durable and is the restore target for a rollback —
    // including across a restart (a fresh manager reads the same dir).
    let mut m2 = ShadowEvolution::new(fast_cfg(false, "accept"), &refs);
    let rollback = m2
        .rollback("alpha", 400_000)
        .expect("cross-restart rollback must find the promotion");
    match rollback {
        EvolutionOutcome::RolledBack { strategy, .. } => {
            assert_eq!(strategy, "alpha");
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(
        m2.registry().get("alpha", "cap").unwrap(),
        dec!(0.40),
        "restored to the pre-promotion parameters"
    );
}

/// Without an `.env` write-back path the auto switch is runtime-only by
/// design (durable writers are disabled), so a restart falls back to the
/// resolved boot config — with one, the flip lives in `.env` and the restart
/// keeps it (that is `a_switch_flip_writes_back_into_the_dotenv`).
#[test]
fn the_auto_evolve_switch_without_an_env_path_falls_back_to_the_boot_config() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "autoswitch");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    assert!(!m.auto_evolve(), "the resolved boot config starts manual");
    m.set_auto_evolve(true);
    assert!(m.auto_evolve(), "the in-session flip is in force");

    // No env path was configured, so the flip had nowhere durable to land:
    // the next boot resolves the switch from config again.
    let m2 = ShadowEvolution::new(fast_cfg(false, "autoswitch"), &refs);
    assert!(
        !m2.auto_evolve(),
        "runtime-only flip: the resolved boot config decides at the next boot"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The operator's ruling: the resolved boot config is the single source of
/// truth for the switches. A legacy `state.json` that carries conflicting
/// switch keys is read past, not honored — the exact production sample
/// (config OFF, state.json `{autoEvolve:true, enabled:true}`) boots OFF
/// with a log line, and the panel's "配置文件与运行期开关不一致" banner
/// can never describe reality again.
#[test]
fn a_legacy_state_json_switch_conflict_is_ignored_the_boot_config_wins() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    // The resolved boot config says OFF; an older kernel's panel flip said ON
    // and was persisted in state.json.
    let cfg = fast_cfg(false, "conflict");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    proposal::ProposalStore::new(&dir).save_state(proposal::PersistedState {
        enabled: Some(true),
        auto_evolve: Some(true),
        last_cycle_ms: 0,
        cycle_seq: 0,
    });
    let m = ShadowEvolution::new(cfg, &refs);
    assert!(
        !m.is_enabled(),
        "the boot config decides: the legacy state.json switch key is ignored"
    );
    assert!(
        !m.auto_evolve(),
        "the auto switch follows the boot config just as strictly"
    );

    // And the reverse direction: a legacy state that says OFF under a boot
    // config saying ON cannot hold the engine down either.
    let dir2 = std::path::PathBuf::from(&fast_cfg(true, "conflict-off").audit_dir);
    let _ = std::fs::remove_dir_all(&dir2);
    proposal::ProposalStore::new(&dir2).save_state(proposal::PersistedState {
        enabled: Some(false),
        auto_evolve: Some(false),
        last_cycle_ms: 0,
        cycle_seq: 0,
    });
    let m2 = ShadowEvolution::new(fast_cfg(true, "conflict-off"), &refs);
    assert!(
        m2.is_enabled(),
        "the boot config's ON is not undone by legacy state"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// With a write-back path configured, a runtime switch flip persists into
/// the `.env` single source (issue 751: the retired `shadow_evolution.toml`
/// write-back was removed), the bookkeeping keeps flowing to state.json, and
/// the next boot resolves the flip back from `.env`.
#[test]
fn a_switch_flip_writes_back_into_the_dotenv() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let root = std::env::temp_dir().join(format!("bkevo-writeback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let cfg = fast_cfg(false, "writeback");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);

    let env_path = root.join(".env");
    let mut m = ShadowEvolution::new_with_env_path(
        cfg,
        &refs,
        Some(env_path.to_string_lossy().into_owned()),
    );
    m.enable(0);
    m.set_auto_evolve(true);
    // The .env carries both flips…
    let text = std::fs::read_to_string(&env_path).unwrap();
    let get = |k: &str| {
        text.lines().find_map(|l| {
            crate::env_file::parse_line(l)
                .filter(|(kk, _)| kk == k)
                .map(|(_, v)| v)
        })
    };
    assert_eq!(
        get("SHADOW_ENABLED").as_deref(),
        Some("true"),
        "the enable flip landed"
    );
    assert_eq!(
        get("SHADOW_AUTO_EVOLVE").as_deref(),
        Some("true"),
        "the auto flip landed"
    );
    // …and state.json carries only the bookkeeping.
    let st = proposal::ProposalStore::new(&dir).load_state().unwrap();
    assert_eq!(st.enabled, None, "state.json no longer speaks for switches");
    assert_eq!(st.auto_evolve, None);

    // The next boot resolves the switches FROM `.env` (what the launcher and
    // `bin_support/boot` do: SHADOW_* env outranks the legacy toml section),
    // so the flip survived WITHOUT any state.json switch key.
    let mut boot_cfg = fast_cfg(false, "writeback");
    let text = std::fs::read_to_string(&env_path).unwrap();
    let get = |k: &str| {
        text.lines().find_map(|l| {
            crate::env_file::parse_line(l)
                .filter(|(kk, _)| kk == k)
                .map(|(_, v)| v)
        })
    };
    boot_cfg.enabled = get("SHADOW_ENABLED").as_deref() == Some("true");
    boot_cfg.auto_evolve = get("SHADOW_AUTO_EVOLVE").as_deref() == Some("true");
    let m2 = ShadowEvolution::new_with_env_path(
        boot_cfg,
        &refs,
        Some(env_path.to_string_lossy().into_owned()),
    );
    assert!(m2.is_enabled(), "the boot honors the .env the verb wrote");
    assert!(m2.auto_evolve());

    // The bookkeeping file itself: written, readable, switch-free.
    let text = std::fs::read_to_string(dir.join("state.json")).unwrap();
    assert!(
        !text.contains("enabled") && !text.contains("autoEvolve"),
        "state.json is bookkeeping only: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&root);
}

/// Without a write-back path (the backtester, most in-process tests) a
/// switch flip stays runtime-only and nothing is written into whatever
/// checkout the caller happens to run from — the same posture as every
/// other `None`-path durable writer.
#[test]
fn a_switch_flip_without_an_env_path_stays_runtime_only() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    // cwd here is the crate during tests; nothing may appear beside it.
    let mut m = ShadowEvolution::new(fast_cfg(false, "nopath"), &refs);
    m.enable(0);
    assert!(m.is_enabled(), "the in-session switch still works");
    m.disable();
    assert!(!m.is_enabled());
}

/// #249: with the auto switch on there is nobody to answer a held proposal,
/// so the very next evaluation pass adopts the backlog — as `auto`, through
/// the same guards and the same promotion log a hand-clicked acceptance
/// writes. Flipping the switch is not what applies a parameter; the pass
/// that follows is.
#[test]
fn auto_mode_adopts_the_held_backlog_on_the_next_pass() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "autodrain"); // manual mode: the proposal is held
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let held = match &m.evaluate(100_000)[0] {
        EvolutionOutcome::Proposed(p) => p.clone(),
        other => panic!("expected a held proposal, got {other:?}"),
    };
    assert_eq!(m.pending_proposal_count(), 1);

    let before = m.registry().get("alpha", "cap").unwrap();
    m.set_auto_evolve(true);
    assert_eq!(
        m.registry().get("alpha", "cap").unwrap(),
        before,
        "the switch itself applies nothing"
    );

    let outcomes = m.evaluate(200_000);
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, EvolutionOutcome::Applied(s) if s.strategy == "alpha")),
        "the pass that follows the switch adopts the backlog: {outcomes:?}"
    );
    assert_eq!(m.pending_proposal_count(), 0, "auto mode has no backlog");
    assert_eq!(
        m.registry().get("alpha", "cap").unwrap(),
        held.to_params.get("cap").unwrap(),
        "the cell holds the proposal's target"
    );
    let log = std::fs::read_to_string(dir.join("promotions.jsonl")).unwrap();
    assert!(log.contains(&held.id), "the adoption is logged: {log}");
    assert!(
        log.contains("\"decidedBy\":\"auto\""),
        "and recorded as unattended: {log}"
    );
    // #269: every promotion carries the exit-ladder identity it was judged
    // under, so "better" can be traced back to a comparison that means
    // something.
    assert!(
        log.contains(&format!("\"caliber\":\"{}\"", m.exit_caliber())),
        "and stamped with the ladder it was measured under: {log}"
    );
    let decided = m
        .all_proposals(10)
        .into_iter()
        .find(|p| p.id == held.id)
        .unwrap();
    assert_eq!(decided.state, ProposalState::Accepted);
    assert_eq!(decided.decided_by, Some(DecidedBy::Auto));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A backlog entry whose premises moved while it was held (the operator
/// edited the knob in between) must NOT be applied: the drain closes it out
/// as rejected with the guard's reason — recorded, never silently dropped,
/// and never left pending in a mode that has no one to decide it.
#[test]
fn auto_mode_closes_out_a_backlog_entry_whose_premises_moved() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "autostale");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let held = match &m.evaluate(100_000)[0] {
        EvolutionOutcome::Proposed(p) => p.clone(),
        other => panic!("expected a held proposal, got {other:?}"),
    };
    // The operator moves the knob the other way (a legal +2.5% step), so the
    // held proposal's target is now more than one gradient step away.
    let mut p = StrategyParams::new();
    p.set("cap", dec!(0.41));
    m.set_params("alpha", p, 150_000).unwrap();

    m.set_auto_evolve(true);
    let outcomes = m.evaluate(200_000);
    assert!(
        !outcomes
            .iter()
            .any(|o| matches!(o, EvolutionOutcome::Applied(_))),
        "a stale step is not applied: {outcomes:?}"
    );
    assert_eq!(m.pending_proposal_count(), 0, "and is not left pending");
    assert_eq!(
        m.registry().get("alpha", "cap").unwrap(),
        dec!(0.41),
        "the operator's value stands"
    );
    let decided = m
        .all_proposals(10)
        .into_iter()
        .find(|p| p.id == held.id)
        .unwrap();
    assert_eq!(decided.state, ProposalState::Rejected);
    assert_eq!(decided.decided_by, Some(DecidedBy::Auto));
    let _ = std::fs::remove_dir_all(&dir);
}

/// #251: an adoption refused at decision time used to close the proposal
/// with no reason anywhere — the panel said 已拒绝 and the operator could not
/// tell "I said no" from "the gradient lock refused it". The lock that
/// refused it is now on the row, in the audit log, and in the counter the
/// IPC surface reports; a restart reads all of it back from the ledger.
#[test]
fn a_refused_adoption_records_the_lock_that_refused_it() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "reftreason");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let held = match &m.evaluate(100_000)[0] {
        EvolutionOutcome::Proposed(p) => p.clone(),
        other => panic!("expected a held proposal, got {other:?}"),
    };
    assert_eq!(
        held.decided_reason, None,
        "a held proposal has no decision, so no reason"
    );

    // The operator moves the knob while the proposal is held, so its target
    // is more than one gradient step from the parameters now in force. The
    // guards re-run against THOSE, not against the ones it was held on.
    let mut p = StrategyParams::new();
    p.set("cap", dec!(0.41));
    m.set_params("alpha", p, 150_000).unwrap();

    let err = m
        .decide_as(&held.id, Decision::Accept, 200_000, DecidedBy::Auto)
        .expect_err("a stale target must not be adopted");
    assert!(
        err.contains("gradient"),
        "the refusal names the lock: {err}"
    );

    let stored = m
        .all_proposals(10)
        .into_iter()
        .find(|p| p.id == held.id)
        .unwrap();
    assert_eq!(stored.state, ProposalState::Rejected);
    assert_eq!(stored.decided_by, Some(DecidedBy::Auto));
    match stored
        .decided_reason
        .as_ref()
        .expect("a refusal must say why")
    {
        proposal::DecisionReason::GuardFailed { guard, detail } => {
            assert_eq!(guard, "gradient");
            assert!(!detail.is_empty(), "the guard's own words are kept");
        }
        other => panic!("expected a guard refusal, got {other:?}"),
    }
    // The counter the IPC surface reports counts the same refusal the ledger
    // row records: the two readings of this history must not disagree.
    assert_eq!(m.rejected_count("alpha"), 1);
    assert_eq!(m.evolution_count("alpha"), 0);
    let audit = std::fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
    assert!(
        audit.contains("gradient lock no longer holds"),
        "the audit file carries the same refusal: {audit}"
    );

    // A restart folds the ledger back and reports the same numbers — the
    // counters used to reset to 0/0 next to a ledger listing a refusal.
    let m2 = ShadowEvolution::new(fast_cfg(false, "reftreason"), &refs);
    let after = m2
        .all_proposals(10)
        .into_iter()
        .find(|p| p.id == held.id)
        .unwrap();
    assert_eq!(
        after.decided_reason, stored.decided_reason,
        "the reason survives the restart"
    );
    assert_eq!(m2.rejected_count("alpha"), 1);
    assert_eq!(m2.evolution_count("alpha"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #251: an unattended adoption moved the parameters and left NO ledger row,
/// so the panel's 台账 was empty while the strategy was in fact running a new
/// version. It is recorded now, as an accepted proposal decided by `auto` —
/// and the counters, the ledger and the promotion log all agree about it.
#[test]
fn an_unattended_adoption_is_a_ledger_row_that_survives_a_restart() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(true, "autoledger");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    // `enable` re-anchors the twins on the clock the test drives, so the
    // synthetic `now` below is not in their past (#250 made the birth stamp
    // real, and a variant born "later" than the tick never qualifies).
    m.enable(0);
    m.set_auto_evolve(true);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let outcomes = m.evaluate(100_000);
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, EvolutionOutcome::Applied(_))),
        "auto mode adopts: {outcomes:?}"
    );

    let row = m
        .all_proposals(10)
        .into_iter()
        .find(|p| p.decided_by == Some(DecidedBy::Auto))
        .expect("an unattended adoption leaves a ledger row");
    assert_eq!(row.state, ProposalState::Accepted);
    assert!(
        row.id.starts_with("auto-"),
        "and is never mistakable for a decided held proposal: {}",
        row.id
    );
    assert_eq!(
        row.decided_reason, None,
        "an adoption's reason is the comparison block it carries, not a refusal"
    );
    assert_ne!(
        row.from_params.get("cap"),
        row.to_params.get("cap"),
        "the row records a real move"
    );
    assert_eq!(m.evolution_count("alpha"), 1);

    let m2 = ShadowEvolution::new(fast_cfg(true, "autoledger"), &refs);
    assert!(
        m2.all_proposals(10).iter().any(|p| p.id == row.id),
        "the adoption survives the restart"
    );
    assert_eq!(
        m2.evolution_count("alpha"),
        1,
        "and the counter is seeded from the ledger instead of restarting at 0"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #245: an adoption is memory-only during a run, so a restart used to fall
/// back to the declared defaults while the promotion log still reported the
/// adopted value — two "current parameters" readings that disagreed. A fresh
/// manager over the same audit dir restores what was in force, and the
/// one-click rollback keeps working behind it.
#[test]
fn a_restart_restores_the_adopted_parameters() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let cfg = fast_cfg(false, "restore");
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    drive_wins(&mut m, "alpha", 2, 10_000);
    let held = match &m.evaluate(100_000)[0] {
        EvolutionOutcome::Proposed(p) => p.clone(),
        other => panic!("expected a held proposal, got {other:?}"),
    };
    m.decide(&held.id, Decision::Accept, 200_000).unwrap();
    let adopted = m.registry().get("alpha", "cap").unwrap();
    assert_ne!(adopted, dec!(0.40), "the acceptance moved the knob");

    // The restart publishes the declared default first and then re-applies
    // the promotion, so the two readings agree again.
    let m2 = ShadowEvolution::new(fast_cfg(false, "restore"), &refs);
    assert_eq!(
        m2.registry().get("alpha", "cap").unwrap(),
        adopted,
        "the adopted parameters survive the restart"
    );
    // A third manager still finds the rollback target through the log.
    let mut m3 = ShadowEvolution::new(fast_cfg(false, "restore"), &refs);
    match m3.rollback("alpha", 300_000).unwrap() {
        EvolutionOutcome::RolledBack { to, .. } => {
            assert_eq!(to.get("alpha", "cap"), Some(dec!(0.40)));
        }
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(
        m3.registry().get("alpha", "cap").unwrap(),
        dec!(0.40),
        "and restores the pre-promotion value"
    );
    // A rollback is a decision: the next restart must not re-apply what the
    // operator just undid.
    let m4 = ShadowEvolution::new(fast_cfg(false, "restore"), &refs);
    assert_eq!(
        m4.registry().get("alpha", "cap").unwrap(),
        dec!(0.40),
        "a rolled-back promotion is no longer a restore target"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without an `.env` write-back path the engine switch is runtime-only: the
/// in-session flip is in force, and the next boot falls back to the resolved
/// boot config. The durable flip (the production posture) is
/// `a_switch_flip_writes_back_into_the_dotenv`.
#[test]
fn the_engine_switch_without_an_env_path_falls_back_to_the_boot_config() {
    let (a, _b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a];
    let dir = std::path::PathBuf::from(&fast_cfg(false, "engineswitch").audit_dir);
    let _ = std::fs::remove_dir_all(&dir);

    // The boot config says off, the operator switches it on (runtime-only).
    let mut m = ShadowEvolution::new(fast_cfg(false, "engineswitch"), &refs);
    assert!(!m.is_enabled());
    m.enable(0);
    assert!(m.is_enabled(), "the in-session flip is in force");
    let m2 = ShadowEvolution::new(fast_cfg(false, "engineswitch"), &refs);
    assert!(
        !m2.is_enabled(),
        "no durable writer: the boot config decides at the next boot"
    );
    assert_eq!(
        m2.variant_count(),
        0,
        "no twins behind a boot config that says off"
    );

    // A boot config saying ON arms the engine without any state.json in play.
    let m3 = ShadowEvolution::new(fast_cfg(true, "engineswitch"), &refs);
    assert!(m3.is_enabled(), "the boot config's ON stands on its own");
    assert_eq!(m3.variant_count(), 3, "and carries its twins");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The 72h DEEP clock: the first observation starts it, the interval
/// suppresses early firings, and the round itself re-anchors every unit's
/// variant set with COMPOUND mutants (deep-*).
#[test]
fn the_deep_cycle_fires_only_after_the_full_interval() {
    let (a, b) = strategies();
    let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
    let mut cfg = fast_cfg(false, "cycle");
    cfg.evolution_cycle_secs = 100;
    cfg.deep_dims = 2;
    let dir = std::path::PathBuf::from(&cfg.audit_dir);
    let _ = std::fs::remove_dir_all(&dir);
    let mut m = ShadowEvolution::new(cfg, &refs);
    m.enable(0);
    m.on_round(&[evo_market()], &[], 0);
    m.on_tick("t", &evo_book(0.43, 0.45), 1_000);

    // First call starts the clock: no round, and nothing is scheduled yet
    // (the next-fire time is only known once the clock is running).
    assert!(m.maybe_evolution_cycle(2_000).is_none());
    assert!(m.next_cycle_at_ms(2_000).is_some());
    assert!(
        m.variant_views(2_000)
            .iter()
            .all(|v| !v.label.starts_with("deep-")),
        "no deep round before the interval"
    );

    // Inside the interval: nothing fires.
    assert!(m.maybe_evolution_cycle(50_000).is_none());

    // At the interval boundary (clock started at t=2_000 + 100s): the round
    // fires and re-anchors BOTH strategies with deep-*.
    let event = m
        .maybe_evolution_cycle(102_000)
        .expect("the round fires at the boundary");
    assert_eq!(event.cycle_seq, 1);
    assert_eq!(event.dims, 2);
    assert_eq!(event.strategies.len(), 2);
    let names: Vec<_> = m
        .variant_views(100_001)
        .into_iter()
        .filter(|v| !v.is_baseline)
        .map(|v| v.label)
        .collect();
    assert!(
        names.iter().all(|l| l.starts_with("deep-")),
        "the sets are re-anchored with compound mutants: {names:?}"
    );
    // A second call inside the NEW interval does not fire again.
    assert!(m.maybe_evolution_cycle(150_000).is_none());
    assert_eq!(m.cycle_info().1, 1);
}
