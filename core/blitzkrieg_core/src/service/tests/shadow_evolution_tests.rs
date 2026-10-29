#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use crate::shadow_evolution::EvolutionStatus;
use crate::shadow_evolution::{MutableParams, StrategyParams};
use rust_decimal_macros::dec;

/// A private evolution audit dir for one test, returned as the tuning block
/// that points the manager at it.
///
/// The runtime switches live in `<audit_dir>/state.json` and WIN over the
/// config (#249), so a test that left them on the default `data/evolution`
/// would hand its switches to the next test to run — and inherit whatever
/// that one wrote. Each caller gets its own directory and its own switches,
/// and removes it at the end.
pub(super) fn scratch(tag: &str) -> (Option<ShadowEvolutionTuning>, String) {
    let dir = std::env::temp_dir()
        .join(format!("bkse-{}-{tag}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_dir_all(&dir);
    (
        Some(ShadowEvolutionTuning {
            audit_dir: Some(dir.clone()),
            ..Default::default()
        }),
        dir,
    )
}

/// One strategy's override bag, built the way the IPC handler builds it.
fn bag(strategy: &str, knob: &str, value: Decimal) -> MutableParams {
    let mut p = StrategyParams::new();
    p.set(knob, value);
    let mut m = MutableParams::new();
    m.set_strategy(strategy, p);
    m
}

/// The `spread_arb` cap currently in force, read through the strategy's own
/// config view (the same surface an external cdylib reports through
/// `bk_strategy_config_view`) — the kernel no longer knows the field name.
fn cap(c: &Core) -> Decimal {
    let views = c.engine.as_ref().unwrap().strategy_config_views();
    let (_, json) = views
        .iter()
        .find(|(n, _)| n == "spread_arb")
        .expect("test adapter reports a config view");
    serde_json::from_str::<serde_json::Value>(json)
        .unwrap()
        .get("trendMaxEntryPrice")
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str_exact(s).ok())
        .expect("trendMaxEntryPrice in the view")
}

/// A fresh engine with the three test adapters hosted (the kernel ships no
/// strategies, PR-B) — the shape the evolution tests need: one evolvable
/// strategy per declared knob set, registered before `shadow_evolution_enable`.
fn se_engine() -> crate::engine::Engine {
    let mut e = crate::engine::Engine::new(crate::engine::EngineConfig::default());
    crate::strategies::test_support::host(&mut e, Default::default(), Default::default());
    e
}

/// Acceptance: enabling Shadow Evolution attaches the per-strategy hot-swap
/// registry to the engine, and a parameter change is visible to the engine on
/// the next read (no restart). Also: enabling is opt-in, apply is per-strategy
/// and rollback restores the prior set for that strategy only.
#[test]
fn hot_swap_reaches_the_engine_and_rolls_back() {
    let (tuning, dir) = scratch("hotswap");
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig::default(),
        dry_seed_balance: dec!(1000),
        shadow_evolution_enabled: false,
        shadow_evolution_tuning: tuning,
        ..Default::default()
    });
    c.enable_engine(se_engine());
    assert!(!c.shadow_evolution().is_enabled());
    assert!(!c.engine.as_ref().unwrap().has_hot_params());

    // Enable → engine gains the hot-swap handle.
    c.shadow_evolution_enable(1000);
    assert!(c.engine.as_ref().unwrap().has_hot_params());
    assert_eq!(
        c.shadow_evolution().strategy_names(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string()
        ],
        "every hosted strategy that declares knobs is evolved; the chase leg \
         starts disabled but its declaration is read off the live instance"
    );

    let before = cap(&c);
    // Simulate an applied evolution via the operator override path (+3%).
    c.shadow_evolution_apply(
        bag("spread_arb", "trend_max_entry_price", before * dec!(1.03)),
        1500,
    )
    .unwrap();
    assert_eq!(
        cap(&c),
        before * dec!(1.03),
        "engine must observe hot-swapped params"
    );

    // Rollback restores the previous set — for that strategy only.
    c.shadow_evolution_rollback("spread_arb", 2000).unwrap();
    assert_eq!(
        cap(&c),
        before,
        "rollback must restore prior params in the engine"
    );
    assert_eq!(
        c.shadow_evolution().evolution_count("spread_arb"),
        0,
        "rollback is not an evolution"
    );

    // A manual apply is now traced; the audit is per strategy.
    let recs = c.shadow_evolution_history(Some("spread_arb"), 10);
    assert_eq!(recs.len(), 2, "manual apply + rollback");
    assert!(recs[0].manual);
    assert!(recs.iter().all(|r| r.strategy == "spread_arb"));
    assert!(c.shadow_evolution_history(Some("nope"), 10).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// #250: every variant born by the enable path carries the clock the caller
/// passed. `age_sec` is what the evaluator's observation floor
/// (`min_observation_secs`) measures against, so a birth stamp of 0 does not
/// merely mislabel a timestamp — against a wall-clock `now` the variant
/// reports an age of decades, and the floor stops gating anything: a twin
/// seconds old would qualify the moment its trade count allowed it.
///
/// The placeholder lived in the re-registration the enable path triggers (the
/// hot-param rewire), which is why this asserts on the whole path rather than
/// on `scaffold` alone.
#[test]
fn enabling_stamps_variant_birth_with_the_clock_it_was_given() {
    let (tuning, dir) = scratch("birth-stamp");
    let mut c = Core::new(CoreConfig {
        dry_seed_balance: dec!(1000),
        shadow_evolution_enabled: false,
        shadow_evolution_tuning: tuning,
        ..Default::default()
    });
    c.enable_engine(se_engine());
    // A wall-clock-shaped instant, not a small synthetic one: the defect this
    // guards against is only visible against a realistic clock.
    let now = 1_790_003_600_000;
    c.shadow_evolution_enable(now);

    let views = c.shadow_evolution_variants(now);
    assert_eq!(
        views.len(),
        9,
        "3 evolvable strategies × (baseline + 2 single-knob variants)"
    );
    for v in &views {
        assert_eq!(
            v.age_sec, 0,
            "{} / {} was stamped with a placeholder instead of the enable clock {now}",
            v.strategy, v.id
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Apply names exactly one strategy: a multi-strategy bag is refused rather
/// than partially applied, and an unknown strategy is an error (not a silent
/// no-op that another strategy's change could be mistaken for).
#[test]
fn apply_and_rollback_are_per_strategy() {
    let (tuning, dir) = scratch("per-strategy");
    let mut c = Core::new(CoreConfig {
        dry_seed_balance: dec!(1000),
        shadow_evolution_tuning: tuning,
        ..Default::default()
    });
    c.enable_engine(se_engine());
    c.shadow_evolution_enable(0);

    let before = cap(&c);
    let mut both = bag("spread_arb", "trend_max_entry_price", before * dec!(1.03));
    both.set_strategy("alpha_strategy", StrategyParams::new());
    assert!(
        c.shadow_evolution_apply(both, 1000).is_err(),
        "one strategy at a time"
    );
    assert_eq!(cap(&c), before, "a refused apply must not move anything");

    assert!(
        c.shadow_evolution_apply(bag("nope", "x", dec!(1)), 1100)
            .is_err()
    );
    assert!(c.shadow_evolution_rollback("nope", 1200).is_err());
    assert!(
        c.shadow_evolution_rollback("spread_arb", 1300).is_err(),
        "nothing to roll back yet"
    );

    // An undeclared knob for spread_arb is refused: a proposal can never
    // smuggle in a field the strategy did not open to evolution.
    assert!(
        c.shadow_evolution_apply(bag("spread_arb", "hard_stop_loss_pct", dec!(1)), 1400)
            .is_err()
    );
    assert_eq!(cap(&c), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disabled_by_default_is_fully_inert() {
    let (tuning, dir) = scratch("inert");
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig::default(),
        shadow_evolution_tuning: tuning,
        ..Default::default()
    });
    c.enable_engine(se_engine());
    let before = cap(&c);
    assert!(!c.shadow_evolution().is_enabled());
    assert_eq!(c.shadow_evolution().variant_count(), 0);
    assert!(
        !c.engine.as_ref().unwrap().has_hot_params(),
        "no overlay while disabled"
    );
    c.shadow_evolution_evaluate(1000);
    assert_eq!(c.shadow_evolution_history(None, 10).len(), 0);
    // Nothing is registered and nothing moves, so the strategy runs on the
    // config the host pushed — the pre-evolution behaviour exactly.
    assert!(c.shadow_evolution().strategy_names().is_empty());
    assert!(c.shadow_evolution().current_params().is_empty());
    assert!(c.shadow_evolution_status_for("spread_arb", 0).is_none());
    assert_eq!(c.shadow_evolution_status(0), EvolutionStatus::Disabled);
    c.shadow_evolution_apply(bag("spread_arb", "trend_max_entry_price", dec!(0.50)), 1200)
        .unwrap_err();
    assert_eq!(cap(&c), before, "a disabled engine cannot be written to");

    // Enabling registers the declared names and attaches the overlay; the
    // published values are the declaration, not an invented parameter.
    c.shadow_evolution_enable(2000);
    assert_eq!(
        c.shadow_evolution().strategy_names(),
        vec![
            "spread_arb".to_string(),
            "trend_follow".to_string(),
            "mean_reversion".to_string()
        ],
        "E4-a hosts a second evolvable builtin; both declare knobs"
    );
    assert!(c.engine.as_ref().unwrap().has_hot_params());
    assert_eq!(
        c.shadow_evolution()
            .current_params()
            .get("spread_arb", "trend_max_entry_price"),
        Some(before),
    );
    assert_eq!(
        cap(&c),
        before,
        "attaching the overlay must not itself move a value"
    );

    // Disabling DETACHES it again: back to the untouched base config.
    c.shadow_evolution_apply(
        bag("spread_arb", "trend_max_entry_price", before * dec!(1.03)),
        2100,
    )
    .unwrap();
    assert_ne!(cap(&c), before);
    c.shadow_evolution_disable();
    assert!(!c.engine.as_ref().unwrap().has_hot_params());
    assert_eq!(
        cap(&c),
        before,
        "disabling restores the base config exactly"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
