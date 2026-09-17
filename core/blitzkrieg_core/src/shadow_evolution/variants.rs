//! Shadow Evolution — variants: one strategy twin per mutated parameter set.
//!
//! A variant is no longer a kernel-side re-implementation of an entry rule. It
//! is a **twin of one strategy** ([`crate::strategies::shadow_twin::TwinReplay`])
//! driven with a counterfactual parameter set, so the decision logic under
//! comparison is the strategy's own (E2-c / #28). Identical tick stream,
//! identical machinery, one knob different — the counterfactual is real.
//!
//! Mutation is DIRECTED (D-3): each variant moves exactly ONE declared knob, so
//! any edge is attributable to a single parameter. It is deterministic (no RNG)
//! and swept by an offset, so successive re-anchors visit every knob on both
//! sides instead of re-testing the same one forever.

use super::config::KnobSpec;
use super::knobs::StrategyParams;
use crate::exit_policy::ExitConfig;
use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::strategies::EngineStrategy;
use crate::strategies::shadow_twin::{ShadowFactory, ShadowTickCtx, TwinReplay};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Performance metrics over an evaluation window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    pub sample_count: u32,
    pub wins: u32,
    pub gross_profit: Decimal,
    pub gross_loss: Decimal,
    pub total_pnl: Decimal,
}

impl Metrics {
    pub fn win_rate(&self) -> Decimal {
        if self.sample_count == 0 {
            Decimal::ZERO
        } else {
            Decimal::from(self.wins) / Decimal::from(self.sample_count)
        }
    }
    /// gross profit / gross loss; capped at 100 when there are no losses so the
    /// value stays comparable rather than exploding.
    pub fn profit_factor(&self) -> Decimal {
        if self.gross_loss <= Decimal::ZERO {
            if self.gross_profit > Decimal::ZERO {
                Decimal::from(100)
            } else {
                Decimal::ZERO
            }
        } else {
            self.gross_profit / self.gross_loss
        }
    }

    /// Fold a windowed trade list into metrics. The single definition of the
    /// metric, used for both the baseline and every variant.
    pub fn from_trades(trades: &[(i64, Decimal)]) -> Self {
        let mut m = Self::default();
        for (_, pnl) in trades {
            m.sample_count += 1;
            m.total_pnl += *pnl;
            if *pnl > Decimal::ZERO {
                m.wins += 1;
                m.gross_profit += *pnl;
            } else {
                m.gross_loss += -*pnl;
            }
        }
        m
    }
}

/// How much one directed variant step moves its knob (3%, inside the ±5% lock).
const VARIANT_STEP: Decimal = dec!(0.03);

/// One shadow variant: a strategy twin plus the parameter set it runs.
pub struct Variant {
    pub id: String,
    pub label: String,
    pub params: StrategyParams,
    pub is_baseline: bool,
    pub created_at_ms: i64,
    pub crashed: bool,
    replay: TwinReplay,
}

impl Variant {
    /// Build a variant around a twin built from `params`. `None` when the
    /// strategy could not produce a twin (then there is nothing to compare and
    /// the caller reports the strategy as not evolvable).
    pub fn new(
        id: String,
        label: String,
        params: StrategyParams,
        is_baseline: bool,
        now_ms: i64,
        factory: &dyn ShadowFactory,
        exit_cfg: &ExitConfig,
    ) -> Option<Self> {
        let twin = factory.make(&params)?;
        Some(Self::from_twin(
            id,
            label,
            params,
            is_baseline,
            now_ms,
            twin,
            exit_cfg,
        ))
    }

    /// Test/observability constructor from an already-built twin.
    #[allow(clippy::too_many_arguments)]
    pub fn from_twin(
        id: String,
        label: String,
        params: StrategyParams,
        is_baseline: bool,
        now_ms: i64,
        twin: Box<dyn EngineStrategy>,
        exit_cfg: &ExitConfig,
    ) -> Self {
        Self {
            id,
            label,
            params,
            is_baseline,
            created_at_ms: now_ms,
            crashed: false,
            replay: TwinReplay::new(twin, exit_cfg),
        }
    }

    pub fn on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.replay.on_round(markets, seeds, now_ms);
    }

    /// Advance one tick. Returns whether the twin's OWN code panicked (KI-23) —
    /// the panic is absorbed inside the replay, but the caller must latch
    /// `crashed` so a permanently panicking variant stops being re-ticked.
    pub fn on_tick(&mut self, ctx: &ShadowTickCtx<'_>) -> bool {
        self.replay.on_tick(ctx)
    }

    pub fn open_positions(&self) -> usize {
        self.replay.open_positions()
    }

    pub fn metrics(&mut self, window_secs: i64, now_ms: i64) -> Metrics {
        let trades = self.replay.windowed_trades(window_secs, now_ms);
        Metrics::from_trades(&trades)
    }

    pub fn age_sec(&self, now_ms: i64) -> i64 {
        (now_ms - self.created_at_ms) / 1000
    }
}

/// A strategy's whole shadow set: the baseline (parameters in force) plus
/// `count-1` directed single-knob variants.
pub struct VariantSet {
    pub strategy: String,
    pub variants: Vec<Variant>,
}

impl VariantSet {
    /// A set that observes nothing — the disabled / not-yet-scaffolded state.
    pub fn empty(strategy: &str) -> Self {
        Self {
            strategy: strategy.to_string(),
            variants: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.variants.len()
    }
    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    pub fn baseline_metrics(&mut self, window_secs: i64, now_ms: i64) -> Metrics {
        match self.variants.iter_mut().find(|v| v.is_baseline) {
            Some(b) => b.metrics(window_secs, now_ms),
            None => Metrics::default(),
        }
    }

    /// Advance the round for every variant (resets per-round state and drops
    /// virtual positions whose market left the round, which live force-exits).
    /// Closed-trade history is kept so metrics accumulate across round
    /// boundaries (D-3) instead of resetting every 900s round.
    pub fn on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, OrderbookSnapshot)],
        now_ms: i64,
    ) {
        for v in self.variants.iter_mut() {
            v.on_round(markets, seeds, now_ms);
        }
    }
}

/// Build a strategy's variant set: baseline (parameters in force) plus
/// `count-1` directed single-knob variants.
///
/// Returns an empty set when the strategy declares no knobs or cannot build a
/// twin — in both cases there is no honest counterfactual to run, and the caller
/// records the strategy as not evolvable rather than comparing it against
/// itself.
///
/// `sweep` is an anchor offset (the strategy's applied-evolution count): variant
/// `i` takes directed move `m = sweep + i - 1`, which pairs up as
/// `knob = (m / 2) % n`, `up = m % 2 == 0`. Two consecutive moves therefore visit
/// ONE knob in BOTH directions before moving on, and because `m` advances by the
/// number of variants on every re-anchor, successive epochs start at a different
/// knob. (An earlier scheme derived the direction from `m`'s parity while the
/// knob was `m % n` — with an even `n` that pinned every knob to a single
/// direction forever, so half the search space was unreachable.)
///
/// Direction is a mechanical exploration choice, NOT a semantic claim about the
/// knob: a variant only ever qualifies by improving the metrics, so a
/// wrong-direction variant simply never wins. Both directions must be reachable
/// for that argument to hold.
#[allow(clippy::too_many_arguments)]
pub fn build_variants(
    strategy: &str,
    specs: &[KnobSpec],
    base: &StrategyParams,
    factory: &dyn ShadowFactory,
    count: usize,
    max_gradient: Decimal,
    exit_cfg: &ExitConfig,
    now_ms: i64,
    sweep: u64,
) -> VariantSet {
    let mut out = VariantSet::empty(strategy);
    // A degenerate declaration (a zero-width domain) leaves nothing to mutate;
    // running the baseline alone would make the comparison vacuous.
    let mutable: Vec<&KnobSpec> = specs.iter().filter(|k| k.min < k.max).collect();
    if mutable.is_empty() {
        return out;
    }
    let Some(baseline) = Variant::new(
        "baseline".into(),
        "live(baseline)".into(),
        base.clone(),
        true,
        now_ms,
        factory,
        exit_cfg,
    ) else {
        return out;
    };
    out.variants.push(baseline);

    let n = mutable.len();
    for i in 1..count {
        let m = sweep as usize + i - 1;
        let spec = mutable[(m / 2) % n];
        let factor = if m % 2 == 0 {
            Decimal::ONE + VARIANT_STEP
        } else {
            Decimal::ONE - VARIANT_STEP
        };

        let mut want = base.clone();
        // Step from the value IN FORCE, not from the value the strategy happened
        // to declare when it was loaded: after an applied evolution the two
        // differ, and a variant is defined as a ±3% move on the CURRENT value.
        let current = base.get(&spec.name).unwrap_or(spec.value);
        want.set(&spec.name, current * factor);
        let params = super::guard::clamped_step(base, &want, max_gradient, specs);

        let Some(v) = Variant::new(
            format!("variant-{i}"),
            format!("variant-{i}({} x{factor})", spec.name),
            params,
            false,
            now_ms,
            factory,
            exit_cfg,
        ) else {
            continue;
        };
        out.variants.push(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow_evolution::knobs::KnobSpec;
    use crate::signal::{SpreadArbConfig, TrendConfig};
    use crate::strategies::shadow_twin::tick_ctx;
    use crate::strategies::spread_arb::{SpreadArbBuiltin, spread_arb_knobs};
    use rust_decimal::prelude::FromPrimitive;

    fn specs() -> Vec<KnobSpec> {
        spread_arb_knobs(&SpreadArbConfig::default())
    }

    fn factory() -> Box<dyn ShadowFactory> {
        SpreadArbBuiltin::new(TrendConfig::default(), SpreadArbConfig::default())
            .shadow_factory()
            .unwrap()
    }

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels(
            "t",
            vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
            vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
            0,
        )
    }

    fn market() -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "t".into(),
            down_token_id: "t-down".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 900_000,
            round_slot: 1,
            neg_risk: false,
            question: "?".into(),
        }
    }

    #[test]
    fn build_produces_a_baseline_plus_directed_single_knob_variants() {
        let base = StrategyParams::from_knobs(&specs());
        let f = factory();
        let set = build_variants(
            "spread_arb",
            &specs(),
            &base,
            f.as_ref(),
            4,
            dec!(0.05),
            &ExitConfig::default(),
            0,
            0,
        );
        assert_eq!(set.variants.len(), 4);
        assert!(set.variants[0].is_baseline);
        for v in set.variants.iter().skip(1) {
            let changed = base
                .iter()
                .filter(|(n, val)| v.params.get(n) != Some(*val))
                .count();
            assert_eq!(changed, 1, "variant {} changed {changed} knobs", v.id);
            assert!(super::super::guard::validate_gradient(&base, &v.params, dec!(0.05)).is_ok());
            assert!(super::super::guard::validate_domain(&v.params, &specs()).is_ok());
        }
    }

    #[test]
    fn the_sweep_visits_every_knob_and_both_directions() {
        let base = StrategyParams::from_knobs(&specs());
        let f = factory();
        let mut moved: Vec<(String, bool)> = Vec::new(); // (knob, moved up?)
        for sweep in 0..8u64 {
            let set = build_variants(
                "s",
                &specs(),
                &base,
                f.as_ref(),
                3,
                dec!(0.05),
                &ExitConfig::default(),
                0,
                sweep,
            );
            for v in set.variants.iter().skip(1) {
                for (name, val) in v.params.iter() {
                    let old = base.get(name).unwrap();
                    if val != old {
                        moved.push((name.to_string(), val > old));
                    }
                }
            }
        }
        for spec in specs() {
            let hits: Vec<bool> = moved
                .iter()
                .filter(|(n, _)| *n == spec.name)
                .map(|(_, up)| *up)
                .collect();
            assert!(hits.contains(&true), "{} never explored upward", spec.name);
            assert!(
                hits.contains(&false),
                "{} never explored downward",
                spec.name
            );
        }
    }

    #[test]
    fn no_declaration_means_no_variants_at_all() {
        let base = StrategyParams::new();
        let f = factory();
        let set = build_variants(
            "spread_arb",
            &[],
            &base,
            f.as_ref(),
            4,
            dec!(0.05),
            &ExitConfig::default(),
            0,
            0,
        );
        assert!(set.is_empty(), "nothing declared ⇒ nothing to compare");
    }

    #[test]
    fn a_zero_width_domain_is_not_mutable() {
        let specs = vec![KnobSpec::new("fixed", dec!(1), dec!(1), dec!(1))];
        let base = StrategyParams::from_knobs(&specs);
        let f = factory();
        let set = build_variants(
            "s",
            &specs,
            &base,
            f.as_ref(),
            3,
            dec!(0.05),
            &ExitConfig::default(),
            0,
            0,
        );
        assert!(
            set.is_empty(),
            "a knob that cannot move cannot produce a variant"
        );
    }

    #[test]
    fn metrics_are_windowed_and_identical_for_baseline_and_variant() {
        let base = StrategyParams::from_knobs(&specs());
        let f = factory();
        let mut set = build_variants(
            "spread_arb",
            &specs(),
            &base,
            f.as_ref(),
            2,
            dec!(0.05),
            &ExitConfig::default(),
            0,
            0,
        );
        let m = market();
        set.on_round(&[m.clone()], &[], 0);
        // Confirm the trend, dip in, run up, exit — for every variant.
        let mut now = 0i64;
        for _ in 0..70 {
            now += 1_000;
            for v in set.variants.iter_mut() {
                let b = book(0.60, 0.62);
                v.on_tick(&tick_ctx(&[m.clone()], "t", &b, 1, 880, now));
            }
        }
        now += 1_000;
        for v in set.variants.iter_mut() {
            let b = book(0.43, 0.45);
            v.on_tick(&tick_ctx(&[m.clone()], "t", &b, 1, 870, now));
        }
        now += 1_000;
        for v in set.variants.iter_mut() {
            let b = book(0.95, 0.97);
            v.on_tick(&tick_ctx(&[m.clone()], "t", &b, 1, 860, now));
        }
        let bm = set.baseline_metrics(1800, now);
        assert_eq!(bm.sample_count, 1, "the baseline traded once");
        assert_eq!(bm.wins, 1);
        // Variant-1 (sweep 0) moves trend_min_price up 3%; confirmation still
        // happens above the raised threshold, so it trades the same dip.
        let v1 = set.variants[1].metrics(1800, now);
        assert_eq!(v1.sample_count, 1);
        assert!(bm.total_pnl > Decimal::ZERO);
    }

    #[test]
    fn variants_are_deterministic() {
        let base = StrategyParams::from_knobs(&specs());
        let f = factory();
        let a = build_variants(
            "s",
            &specs(),
            &base,
            f.as_ref(),
            4,
            dec!(0.05),
            &ExitConfig::default(),
            0,
            3,
        );
        let b = build_variants(
            "s",
            &specs(),
            &base,
            f.as_ref(),
            4,
            dec!(0.05),
            &ExitConfig::default(),
            0,
            3,
        );
        let ap: Vec<_> = a.variants.iter().map(|v| v.params.clone()).collect();
        let bp: Vec<_> = b.variants.iter().map(|v| v.params.clone()).collect();
        assert_eq!(ap, bp);
    }
}
