//! Shadow Evolution — the evaluator that decides when to evolve.
//!
//! All trigger conditions MUST hold (per spec):
//!   1. variant sample count  >= `min_sample_count` (>=30)
//!   2. baseline sample count >= `min_sample_count`
//!   3. variant win_rate      >  baseline win_rate + `min_win_rate_improvement`
//!   4. variant profit_factor >  baseline profit_factor * (1 + `min_profit_factor_improvement`)
//!   5. variant observed       >= `min_observation_secs` (>=5 min)
//!   6. time since last apply  >= `cooldown_secs` (>=10 min)

use super::config::ShadowEvolutionConfig;
use super::signal::{EvolutionReason, EvolveSignal};
use super::variants::{Metrics, Variant};
use rust_decimal::Decimal;

/// Outcome of one evaluation pass.
pub struct Selection {
    pub signal: EvolveSignal,
    /// Index into the variant slice.
    pub variant_index: usize,
}

/// Evaluate all variants against the baseline. Returns the best qualifying
/// proposal, or None when no condition set is met.
pub fn evaluate(
    cfg: &ShadowEvolutionConfig,
    baseline: &Metrics,
    variants: &mut [Variant],
    now_ms: i64,
    last_evolution_ms: i64,
) -> Option<Selection> {
    // Condition 6: cooldown.
    if last_evolution_ms > 0 && now_ms - last_evolution_ms < cfg.cooldown_secs * 1000 {
        return None;
    }
    // Condition 2: the baseline needs enough samples to be a fair reference.
    if baseline.sample_count < cfg.min_sample_count {
        return None;
    }

    let base_wr = baseline.win_rate();
    let base_pf = baseline.profit_factor();

    let mut best: Option<(usize, Decimal, EvolutionReason, Decimal)> = None;

    for (i, v) in variants.iter_mut().enumerate() {
        if v.is_baseline || v.crashed {
            continue;
        }
        // Condition 5: observation time.
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
        let pf_ok = m.profit_factor() > base_pf * (Decimal::ONE + cfg.min_profit_factor_improvement);
        if !wr_ok || !pf_ok {
            continue;
        }

        let reason = if wr_ok && pf_ok {
            EvolutionReason::CombinedImprovement
        } else if wr_ok {
            EvolutionReason::HigherWinRate
        } else {
            EvolutionReason::BetterProfitFactor
        };
        // Rank by total PnL among qualifiers.
        let score = m.total_pnl;
        if best.as_ref().map(|(_, s, _, _)| score > *s).unwrap_or(true) {
            let wr_delta = m.win_rate() - base_wr;
            best = Some((i, score, reason, wr_delta));
        }
    }

    let (idx, _score, reason, wr_delta) = best?;

    // Confidence: all conditions passed → at least 0.5; scale up with how far the
    // win-rate improvement exceeds the floor.
    let escalate = if cfg.min_win_rate_improvement > Decimal::ZERO {
        (wr_delta / (cfg.min_win_rate_improvement * Decimal::from(3))).min(Decimal::ONE)
    } else {
        Decimal::ONE
    };
    let confidence = Decimal::new(5, 1) + Decimal::new(5, 1) * escalate;

    let from_params = variants
        .iter()
        .find(|v| v.is_baseline)
        .map(|v| v.params.clone())
        .unwrap_or_default();
    let to_params = variants[idx].params.clone();

    let signal = EvolveSignal::new(
        format!("evolve-{now_ms}"),
        now_ms,
        from_params,
        to_params,
        reason,
        confidence,
        variants[idx].metrics(cfg.evaluation_window_secs, now_ms).sample_count,
        wr_delta,
        variants[idx].id.clone(),
    );
    Some(Selection { signal, variant_index: idx })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::config::MutableParams;
    use super::super::variants::{build_variants, Variant};
    use crate::exit_policy::ExitConfig;
    use rust_decimal_macros::dec;

    /// Drive a variant with a scripted price path to produce a known PnL.
    fn drive(v: &mut Variant, entries: &[(f64, f64)], start_ms: i64) {
        use rust_decimal::prelude::FromPrimitive;
        let mut t = start_ms;
        for i in 0..entries.len() {
            let (bid, ask) = entries[i];
            let book = crate::model::OrderbookSnapshot::from_levels(
                "t",
                vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
                vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
                t,
            );
            v.on_tick("t", &book, true, start_ms + 900_000, t);
            t += 1000;
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

    #[test]
    fn no_signal_when_baseline_has_too_few_samples() {
        let exit = ExitConfig::default();
        let base = MutableParams::default();
        let mut vs = build_variants(&base, 2, dec!(0.05), &exit, 0);
        let baseline_metrics = Metrics::default(); // 0 samples
        assert!(evaluate(&cfg(), &baseline_metrics, &mut vs, 1000, 0).is_none());
    }

    #[test]
    fn emits_signal_when_a_variant_clearly_wins() {
        let exit = ExitConfig::default();
        let base = MutableParams::default();
        // Baseline: one winner, one loser (50% WR).
        let mut vs = build_variants(&base, 2, dec!(0.05), &exit, 0);
        // Variant: two winners (100% WR, higher PF).
        drive(
            &mut vs[1],
            &[
                (0.43, 0.45),
                (0.95, 0.97), // win
                (0.43, 0.45),
                (0.95, 0.97), // win
            ],
            10_000,
        );
        let baseline_metrics = Metrics {
            sample_count: 2,
            wins: 1,
            gross_profit: dec!(2),
            gross_loss: dec!(2),
            total_pnl: dec!(0),
        };
        // Baseline "variant" also needs samples for fair comparison? The evaluator
        // compares against the baseline METRICS, not the baseline variant.
        let sel = evaluate(&cfg(), &baseline_metrics, &mut vs, 20_000, 0);
        assert!(sel.is_some(), "expected a qualifying evolution signal");
        let sig = sel.unwrap().signal;
        assert!(sig.confidence >= dec!(0.5) && sig.confidence <= dec!(1));
        assert!(sig.expected_improvement > dec!(0.05));
    }

    #[test]
    fn cooldown_suppresses_signals() {
        let exit = ExitConfig::default();
        let base = MutableParams::default();
        let mut vs = build_variants(&base, 2, dec!(0.05), &exit, 0);
        drive(&mut vs[1], &[(0.43, 0.45), (0.95, 0.97), (0.43, 0.45), (0.95, 0.97)], 10_000);
        let mut c = cfg();
        c.cooldown_secs = 600;
        let baseline_metrics = Metrics { sample_count: 2, wins: 1, gross_profit: dec!(2), gross_loss: dec!(2), total_pnl: dec!(0) };
        // Last evolution 100s ago < 600s cooldown → suppressed (positive timeline).
        let now = 1_000_000i64;
        assert!(evaluate(&c, &baseline_metrics, &mut vs, now, now - 100_000).is_none());
        // 700s ago → allowed.
        assert!(evaluate(&c, &baseline_metrics, &mut vs, now, now - 700_000).is_some());
    }
}
