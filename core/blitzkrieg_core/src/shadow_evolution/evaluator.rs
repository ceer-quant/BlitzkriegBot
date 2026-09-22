//! Shadow Evolution — the evaluator that decides when a strategy should evolve.
//!
//! One strategy at a time: the caller hands in that strategy's variant set plus
//! its own cooldown clock, so two strategies evolving in parallel never share a
//! trigger decision (E2-c / #28).
//!
//! All trigger conditions MUST hold (per spec):
//!   1. variant sample count  >= `min_sample_count` (>=30)
//!   2. baseline sample count >= `min_sample_count`
//!   3. variant win_rate      >  baseline win_rate + `min_win_rate_improvement`
//!   4. variant profit_factor >  baseline profit_factor * (1 + `min_profit_factor_improvement`)
//!   5. variant observed      >= `min_observation_secs` (>=5 min)
//!   6. time since last apply >= `cooldown_secs` (>=10 min)

use super::config::ShadowEvolutionConfig;
use super::knobs::{MutableParams, StrategyParams};
use super::signal::{EvolutionReason, EvolveSignal};
use super::variants::{Metrics, VariantSet};
use rust_decimal::Decimal;

/// Outcome of one evaluation pass for ONE strategy.
pub struct Selection {
    pub signal: EvolveSignal,
    /// Index into that strategy's variant slice.
    pub variant_index: usize,
}

/// Evaluate one strategy's variants against its own baseline. Returns the best
/// qualifying proposal, or None when no condition set is met.
pub fn evaluate(
    cfg: &ShadowEvolutionConfig,
    strategy: &str,
    set: &mut VariantSet,
    baseline: &Metrics,
    now_ms: i64,
    last_evolution_ms: i64,
) -> Option<Selection> {
    // Condition 6: this strategy's own cooldown.
    if last_evolution_ms > 0 && now_ms - last_evolution_ms < cfg.cooldown_secs * 1000 {
        return None;
    }
    // Condition 2: the baseline needs enough samples to be a fair reference.
    if baseline.sample_count < cfg.min_sample_count {
        return None;
    }

    let base_wr = baseline.win_rate();
    let base_pf = baseline.profit_factor();

    let mut best: Option<(usize, Decimal, Decimal, u32)> = None;

    for (i, v) in set.variants.iter_mut().enumerate() {
        if v.is_baseline || v.crashed {
            continue;
        }
        // Condition 5: observation time. Meaningful only because the variant's
        // birth stamp is a real clock — see `Variant::age_sec` and #250.
        if v.age_sec(now_ms) < cfg.min_observation_secs {
            continue;
        }
        let m = v.metrics(cfg.evaluation_window_secs, now_ms);
        // Condition 1.
        if m.sample_count < cfg.min_sample_count {
            continue;
        }
        // Conditions 3 & 4.
        let wr_ok = m.win_rate() > base_wr + cfg.min_win_rate_improvement;
        let pf_ok =
            m.profit_factor() > base_pf * (Decimal::ONE + cfg.min_profit_factor_improvement);
        if !wr_ok || !pf_ok {
            continue;
        }
        // Rank by total PnL among qualifiers.
        let score = m.total_pnl;
        if best.as_ref().map(|(_, s, _, _)| score > *s).unwrap_or(true) {
            best = Some((i, score, m.win_rate() - base_wr, m.sample_count));
        }
    }

    let (idx, _score, wr_delta, sample_count) = best?;

    // Confidence: all conditions passed → at least 0.5; scale up with how far the
    // win-rate improvement exceeds the floor.
    let escalate = if cfg.min_win_rate_improvement > Decimal::ZERO {
        (wr_delta / (cfg.min_win_rate_improvement * Decimal::from(3))).min(Decimal::ONE)
    } else {
        Decimal::ONE
    };
    let confidence = Decimal::new(5, 1) + Decimal::new(5, 1) * escalate;

    let from = params_for(strategy, baseline_params(set));
    let to = params_for(strategy, set.variants[idx].params.clone());
    let variant_id = set.variants[idx].id.clone();

    let signal = EvolveSignal::new(
        format!("evolve-{now_ms}"),
        now_ms,
        strategy.to_string(),
        from,
        to,
        // Both conditions held for every qualifier, so the reason is the
        // combined improvement.
        EvolutionReason::CombinedImprovement,
        confidence,
        sample_count,
        wr_delta,
        variant_id,
    );
    Some(Selection {
        signal,
        variant_index: idx,
    })
}

/// The parameters in force for this strategy, as recorded by its baseline twin.
fn baseline_params(set: &VariantSet) -> StrategyParams {
    set.variants
        .iter()
        .find(|v| v.is_baseline)
        .map(|v| v.params.clone())
        .unwrap_or_default()
}

/// Wrap one strategy's parameter bag in the aggregate type the signal/audit
/// surfaces carry. One strategy per signal, so the aggregate holds exactly one
/// entry — which is what keeps a signal from naming another strategy's knobs.
fn params_for(strategy: &str, params: StrategyParams) -> MutableParams {
    let mut out = MutableParams::new();
    out.set_strategy(strategy, params);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
    use crate::shadow_evolution::knobs::KnobSpec;
    use crate::shadow_evolution::variants::build_variants;
    use crate::signal::TradeSignal;
    use crate::strategies::shadow_twin::{ShadowFactory, tick_ctx};
    use crate::strategies::{EngineStrategy, StrategyCtx};
    use rust_decimal::prelude::FromPrimitive;
    use rust_decimal_macros::dec;

    /// A synthetic strategy whose ONE knob decides whether it enters: it buys
    /// whenever the mid is at or under `cap`. Nothing else — no trend, no exit
    /// logic — so a decision difference between two parameter sets can only come
    /// from the knob, and the shared exit policy closes the position.
    struct CapStrategy {
        cap: Decimal,
    }

    struct CapFactory;

    impl ShadowFactory for CapFactory {
        fn strategy(&self) -> String {
            "cap".into()
        }
        fn knobs(&self) -> Vec<KnobSpec> {
            vec![KnobSpec::new("cap", dec!(0.40), dec!(0.05), dec!(0.95))]
        }
        fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
            Some(Box::new(CapStrategy {
                cap: params.get("cap").unwrap_or(dec!(0.40)),
            }))
        }
    }

    impl EngineStrategy for CapStrategy {
        fn name(&self) -> &str {
            "cap"
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
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
                strategy: self.name().to_string(),
                asset: market.asset.clone(),
                direction: SignalDirection::Up,
                token_id: token,
                condition_id: market.condition_id.clone(),
                price: book.mid_price,
                reason: format!("mid {} <= cap {}", book.mid_price, self.cap),
            }]
        }
    }

    fn cfg() -> ShadowEvolutionConfig {
        ShadowEvolutionConfig {
            enabled: true,
            min_sample_count: 2,
            min_win_rate_improvement: dec!(0.05),
            min_profit_factor_improvement: dec!(0.10),
            min_observation_secs: 0,
            cooldown_secs: 0,
            ..Default::default()
        }
    }

    fn market() -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "t".into(),
            down_token_id: "t-d".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 900_000,
            round_slot: 1,
            neg_risk: false,
            question: "?".into(),
        }
    }

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels(
            "t",
            vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
            vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
            0,
        )
    }

    /// Baseline cap 0.40; sweep 0 makes variant-1 cap 0.40 * 1.03 = 0.412. A tick
    /// at mid 0.41 therefore splits the two: only the loosened variant enters.
    fn set_for(count: usize) -> VariantSet {
        let f: Box<dyn ShadowFactory> = Box::new(CapFactory);
        let mut base = StrategyParams::new();
        base.set("cap", dec!(0.40));
        build_variants(
            "cap",
            &f.knobs(),
            &base,
            f.as_ref(),
            count,
            dec!(0.05),
            &cfg().exit_cfg,
            0,
            0,
        )
    }

    /// Enter at a mid only the loosened variant admits, then exit rich: a
    /// guaranteed winner per repetition. F7 fillability: the book is locked
    /// (bid = ask = 0.41) so the offer side sits at the entry price itself —
    /// a mid the twin cannot rest a bid under would never fill.
    fn drive_wins(
        v: &mut super::super::variants::Variant,
        m: &CryptoMarket,
        t0: i64,
        times: usize,
    ) {
        let mut now = t0;
        for _ in 0..times {
            now += 1_000;
            v.on_tick(&tick_ctx(
                std::slice::from_ref(m),
                "t",
                &book(0.41, 0.41),
                1,
                880,
                now,
            )); // mid 0.41 — above the 0.40 baseline cap, under the 0.412 variant
            now += 1_000;
            v.on_tick(&tick_ctx(
                std::slice::from_ref(m),
                "t",
                &book(0.95, 0.97),
                1,
                880,
                now,
            )); // exit
        }
    }

    #[test]
    fn no_signal_when_baseline_has_too_few_samples() {
        let mut vs = set_for(2);
        assert!(evaluate(&cfg(), "cap", &mut vs, &Metrics::default(), 1000, 0).is_none());
    }

    #[test]
    fn the_counterfactual_is_a_real_decision_difference() {
        // Proof that this test can detect what it claims: the baseline cap does
        // not admit the tick, the variant's does.
        let m = market();
        let mut vs = set_for(2);
        vs.on_round(std::slice::from_ref(&m), &[], 0);
        // F7 fillability: the book is locked (bid = ask = 0.41), so the offer
        // side sits exactly at the entry price the twin would rest — the old
        // two-sided book (0.40/0.41) could never have filled that bid.
        let tick = book(0.41, 0.41);
        vs.variants[0].on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &tick,
            1,
            880,
            1_000,
        ));
        assert_eq!(
            vs.variants[0].open_positions(),
            0,
            "cap 0.40 must not buy a 0.41 mid"
        );
        vs.variants[1].on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &tick,
            1,
            880,
            1_000,
        ));
        assert_eq!(
            vs.variants[1].open_positions(),
            1,
            "cap 0.412 must buy the same tick"
        );
    }

    #[test]
    fn emits_a_strategy_tagged_signal_when_a_variant_clearly_wins() {
        let m = market();
        let mut vs = set_for(2);
        vs.on_round(std::slice::from_ref(&m), &[], 0);
        drive_wins(&mut vs.variants[1], &m, 10_000, 2);

        // Baseline reference: 50% win rate (one win, one loss).
        let baseline = Metrics {
            sample_count: 2,
            wins: 1,
            gross_profit: dec!(2),
            gross_loss: dec!(2),
            total_pnl: dec!(0),
        };
        let sel = evaluate(&cfg(), "cap", &mut vs, &baseline, 100_000, 0)
            .expect("expected a qualifying evolution signal");
        let sig = sel.signal;

        assert_eq!(
            sig.strategy, "cap",
            "the signal names the strategy it moves"
        );
        assert!(sig.confidence >= dec!(0.5) && sig.confidence <= dec!(1));
        assert_eq!(sig.expected_improvement, dec!(0.5), "100% - 50% win rate");
        assert_eq!(sig.variant_id, "variant-1");
        assert_eq!(sig.sample_count, 2);

        // The proposal mentions exactly this strategy, and only its own knobs.
        assert_eq!(sig.to_params.strategies(), vec!["cap"]);
        assert_eq!(sig.from_params.strategies(), vec!["cap"]);
        assert_eq!(sig.from_params.get("cap", "cap"), Some(dec!(0.40)));
        assert_eq!(sig.to_params.get("cap", "cap"), Some(dec!(0.412)));
        // And the proposed step is inside the gradient lock (it is a variant
        // parameter set, which is what the manager then re-validates).
        assert!(
            super::super::guard::validate_gradient(
                sig.from_params.for_strategy("cap").unwrap(),
                sig.to_params.for_strategy("cap").unwrap(),
                dec!(0.05),
            )
            .is_ok()
        );
    }

    #[test]
    fn cooldown_suppresses_signals_per_strategy() {
        let m = market();
        let mut vs = set_for(2);
        vs.on_round(std::slice::from_ref(&m), &[], 0);
        drive_wins(&mut vs.variants[1], &m, 10_000, 2);
        let mut c = cfg();
        c.cooldown_secs = 600;
        let baseline = Metrics {
            sample_count: 2,
            wins: 1,
            gross_profit: dec!(2),
            gross_loss: dec!(2),
            total_pnl: dec!(0),
        };
        let now = 1_000_000i64;
        assert!(evaluate(&c, "cap", &mut vs, &baseline, now, now - 100_000).is_none());
        assert!(evaluate(&c, "cap", &mut vs, &baseline, now, now - 700_000).is_some());
    }

    #[test]
    fn an_empty_set_never_signals() {
        let mut empty = VariantSet::empty("cap");
        let baseline = Metrics {
            sample_count: 99,
            wins: 99,
            gross_profit: dec!(1),
            gross_loss: dec!(0),
            total_pnl: dec!(1),
        };
        assert!(evaluate(&cfg(), "cap", &mut empty, &baseline, 1000, 0).is_none());
    }

    #[test]
    fn a_losing_variant_never_qualifies() {
        let m = market();
        let mut vs = set_for(2);
        vs.on_round(std::slice::from_ref(&m), &[], 0);
        // Same trade, but exit at a loss instead of a gain.
        let mut now = 10_000;
        for _ in 0..2 {
            now += 1_000;
            vs.variants[1].on_tick(&tick_ctx(
                std::slice::from_ref(&m),
                "t",
                // Locked book: the offer side sits at the entry price the
                // twin rests (F7 fillability).
                &book(0.41, 0.41),
                1,
                880,
                now,
            ));
            now += 1_000;
            vs.variants[1].on_tick(&tick_ctx(
                std::slice::from_ref(&m),
                "t",
                &book(0.10, 0.12),
                1,
                880,
                now,
            ));
        }
        let baseline = Metrics {
            sample_count: 2,
            wins: 1,
            gross_profit: dec!(2),
            gross_loss: dec!(2),
            total_pnl: dec!(0),
        };
        assert!(evaluate(&cfg(), "cap", &mut vs, &baseline, 100_000, 0).is_none());
    }
}
