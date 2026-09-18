//! The built-in `spread_arb` strategy, moved behind the [`EngineStrategy`]
//! seam in P-1.1.
//!
//! Behaviour is unchanged from when this logic lived directly in `engine.rs`:
//!  - it owns the trend tracker (confirmation window, broken-price detection)
//!  - `find_candidates` mirrors `evaluate_spread_arb` over the round's markets
//!  - Shadow Evolution's per-strategy parameters overlay the base config through
//!    the hot-swap cell, read fresh on every evaluation.
//!
//! E2-c (#28): the four tunables are *declared* by this strategy — names, values
//! and domains ([`SPREAD_ARB_KNOBS`]) — and a twin can be built from any
//! parameter set ([`SpreadArbShadowFactory`]), so the shadow comparison runs
//! this strategy's own logic instead of a kernel-side copy of it.

use super::shadow_twin::ShadowFactory;
use super::{EngineStrategy, StrategyCtx};
use crate::model::OrderbookSnapshot;
use crate::shadow_evolution::{KnobSpec, ParamRegistry, StrategyParams};
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig, TrendTracker, evaluate_spread_arb};
use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::sync::Arc;

/// The knobs `spread_arb` declares evolvable, as `(name, default, min, max)`.
/// See [`spread_arb_knobs`] for how the declaration is derived from the config
/// actually in force.
///
/// The domain is a HARD outer bound: `guard::validate_domain` rejects anything
/// outside it before the ±gradient lock is even consulted, so no number of
/// successive steps can walk a knob past these edges. The bounds are wide (they
/// bracket the whole sensible trading range); the gradient lock is what keeps
/// any individual step small.
pub const SPREAD_ARB_KNOBS: [(&str, Decimal, Decimal, Decimal); 4] = [
    ("trend_min_price", dec!(0.55), dec!(0.50), dec!(0.95)),
    ("trend_entry_factor", dec!(0.98), dec!(0.80), dec!(1.00)),
    ("trend_max_entry_price", dec!(0.45), dec!(0.05), dec!(0.90)),
    ("trend_broken_price", dec!(0.35), dec!(0.01), dec!(0.50)),
];

fn knob_value(cfg: &SpreadArbConfig, name: &str) -> Decimal {
    match name {
        "trend_min_price" => cfg.trend_min_price,
        "trend_entry_factor" => cfg.trend_entry_factor,
        "trend_max_entry_price" => cfg.trend_max_entry_price,
        "trend_broken_price" => cfg.trend_broken_price,
        _ => Decimal::ZERO,
    }
}

/// This strategy's knob declaration derived from the config in force, so it
/// reports reality rather than a compiled-in default.
pub fn spread_arb_knobs(cfg: &SpreadArbConfig) -> Vec<KnobSpec> {
    SPREAD_ARB_KNOBS
        .iter()
        .map(|(name, _default, min, max)| {
            let v = knob_value(cfg, name);
            // A config that starts outside the default domain widens it to
            // include the value in force: declaring a box the strategy does not
            // fit in would make every proposal a domain violation.
            KnobSpec::new(*name, v, (*min).min(v), (*max).max(v))
        })
        .collect()
}

/// Overlay a parameter set on a base config. Names this strategy did not declare
/// are ignored, so a stray key can never write a field it never opened.
pub fn apply_knobs(base: &SpreadArbConfig, params: &StrategyParams) -> SpreadArbConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            "trend_min_price" => cfg.trend_min_price = v,
            "trend_entry_factor" => cfg.trend_entry_factor = v,
            "trend_max_entry_price" => cfg.trend_max_entry_price = v,
            "trend_broken_price" => cfg.trend_broken_price = v,
            _ => {}
        }
    }
    cfg
}

/// Overlay the two trend knobs on the TRACKER's config.
///
/// The confirmation floor (`min_price`) and the regime-break floor
/// (`broken_price`) are gates inside [`TrendTracker`], not fields of
/// [`SpreadArbConfig`] — pre-E2-c a variant's `trend_min_price` only changed the
/// entry reason string while the tracker kept confirming at the live floor, so
/// the knob was decision-neutral and any "evolution" of it measured nothing.
/// A twin (and the live instance once evolution is attached) now drives its own
/// tracker with the counterfactual value, which is what makes the declaration
/// honest.
pub fn apply_trend_knobs(base: &TrendConfig, params: &StrategyParams) -> TrendConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            "trend_min_price" => cfg.min_price = v,
            "trend_broken_price" => cfg.broken_price = v,
            _ => {}
        }
    }
    cfg
}

pub struct SpreadArbBuiltin {
    cfg: SpreadArbConfig,
    trend_cfg: TrendConfig,
    trend: TrendTracker,
    /// This strategy's OWN cell in the host's per-strategy parameter registry
    /// (E2-c). None = evolution inactive, so the base config stands alone.
    hot_params: Option<Arc<ArcSwap<StrategyParams>>>,
}

impl SpreadArbBuiltin {
    pub fn new(trend_cfg: TrendConfig, cfg: SpreadArbConfig) -> Self {
        Self {
            trend: TrendTracker::new(trend_cfg.clone()),
            cfg,
            trend_cfg,
            hot_params: None,
        }
    }

    /// The spread_arb config currently in force: base config overlaid with the
    /// hot-swapped per-strategy parameters when Shadow Evolution is active.
    fn effective_cfg(&self) -> SpreadArbConfig {
        match &self.hot_params {
            Some(h) => apply_knobs(&self.cfg, &h.load()),
            None => self.cfg.clone(),
        }
    }

    /// Push the in-force trend knobs into this instance's OWN tracker. Without
    /// hot parameters the tracker keeps the config it was built with, so a core
    /// that never runs evolution behaves exactly as before.
    fn sync_trend_cfg(&mut self) {
        if let Some(h) = &self.hot_params {
            let cfg = apply_trend_knobs(&self.trend_cfg, &h.load());
            self.trend.set_config(cfg);
        }
    }
}

impl EngineStrategy for SpreadArbBuiltin {
    fn name(&self) -> &str {
        "spread_arb"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        self.sync_trend_cfg();
        self.trend.on_price(token_id, snap.mid_price, now_ms);
    }

    fn on_round(&mut self, slot: i64) {
        self.trend.reset_if_new_round(slot);
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        self.trend.take_broken()
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        self.trend.confirmed_tokens()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let confirmed = self.trend.confirmed_tokens();
        if confirmed.is_empty() {
            return Vec::new();
        }
        let cfg = self.effective_cfg();
        let mut out = Vec::new();
        for market in ctx.markets() {
            let up_book = ctx.fresh_book(&market.up_token_id);
            let down_book = ctx.fresh_book(&market.down_token_id);
            if let Some(sig) = evaluate_spread_arb(
                &market.asset,
                &market.condition_id,
                &market.up_token_id,
                &market.down_token_id,
                up_book.as_ref(),
                down_book.as_ref(),
                &confirmed,
                &cfg,
            ) {
                out.push(sig);
            }
        }
        out
    }

    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let eff = self.effective_cfg();
        let cap = eff.trend_max_entry_price;
        let floor = eff.trend_broken_price;
        self.trend
            .confirmed_tokens()
            .into_iter()
            .map(|t| {
                let mid = ctx.fresh_book(&t).map(|b| b.mid_price).unwrap_or(Decimal::ZERO);
                let entry = mid * eff.trend_entry_factor;
                let in_band = entry > Decimal::ZERO && entry <= cap && mid >= floor;
                serde_json::json!({ "token": t, "mid": mid, "entry": entry, "cap": cap, "inBand": in_band })
            })
            .collect()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        // Resolve only OUR cell: a strategy reads its own namespace, and a
        // strategy that declared no knobs receives None and stays untouched.
        // `None` (evolution disabled) DETACHES the overlay, so the live config is
        // the sole input again — bit-for-bit the pre-evolution behaviour.
        self.hot_params = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        spread_arb_knobs(&self.effective_cfg())
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        // The factory is built from the configs in force WITHOUT the hot overlay,
        // because the overlay is exactly what the factory's parameter argument
        // applies. Handing it an already-overlaid base would make the baseline
        // twin differ from the live instance's own base.
        Some(Box::new(SpreadArbShadowFactory {
            trend_cfg: self.trend_cfg.clone(),
            base: self.cfg.clone(),
        }))
    }

    fn spread_arb_view(&self) -> Option<SpreadArbConfig> {
        Some(self.effective_cfg())
    }

    fn on_config(&mut self, trend: &TrendConfig, spread_arb: &SpreadArbConfig) {
        self.cfg = spread_arb.clone();
        self.trend_cfg = trend.clone();
        self.trend.set_config(trend.clone());
    }
}

/// Builds independent `spread_arb` twins for the shadow engine.
///
/// A twin owns its OWN `TrendTracker` from the same trend config: that is what
/// makes the two trend knobs real inputs to the counterfactual instead of
/// decorations on the entry reason string (they drive confirmation and
/// broken-trend detection).
pub struct SpreadArbShadowFactory {
    trend_cfg: TrendConfig,
    base: SpreadArbConfig,
}

impl ShadowFactory for SpreadArbShadowFactory {
    fn strategy(&self) -> String {
        "spread_arb".to_string()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        spread_arb_knobs(&self.base)
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        // The twin gets its own tracker, configured from the counterfactual trend
        // knobs, so `trend_min_price`/`trend_broken_price` genuinely change what
        // the twin confirms and breaks — not just its reason string.
        Some(Box::new(SpreadArbBuiltin::new(
            apply_trend_knobs(&self.trend_cfg, params),
            apply_knobs(&self.base, params),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::super::shadow_twin::{TwinReplay, tick_ctx};
    use super::*;
    use crate::exit_policy::ExitConfig;
    use crate::model::CryptoMarket;
    use crate::shadow_evolution::variants::Metrics;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        use rust_decimal::prelude::FromPrimitive;
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

    /// Drive `count` above-threshold ticks so the twin's trend confirms, then
    /// return the clock. The confirmation window is 60s (TrendConfig default)
    /// with a 10s floor, so 70 one-second ticks span well past 90% of it.
    fn confirm(replay: &mut TwinReplay, m: &CryptoMarket, from: i64, count: usize) -> i64 {
        let mut now = from;
        for _ in 0..count {
            now += 1_000;
            let b = book(0.60, 0.62);
            replay.on_tick(&tick_ctx(std::slice::from_ref(m), "t", &b, 1, 880, now));
        }
        now
    }

    #[test]
    fn declarations_report_the_config_in_force_and_are_coherent() {
        let k = spread_arb_knobs(&SpreadArbConfig::default());
        assert_eq!(k.len(), 4);
        for spec in &k {
            assert!(spec.is_coherent(), "{spec:?}");
        }
        assert_eq!(k[0].name, "trend_min_price");
        assert_eq!(k[0].value, dec!(0.55));
        // An out-of-domain starting config widens the declared domain instead of
        // declaring a box the strategy cannot fit in.
        let cfg = SpreadArbConfig {
            trend_max_entry_price: dec!(0.99),
            ..Default::default()
        };
        let k = spread_arb_knobs(&cfg);
        let cap = k
            .iter()
            .find(|s| s.name == "trend_max_entry_price")
            .unwrap();
        assert!(cap.contains(dec!(0.99)));
        assert!(cap.is_coherent());
    }

    #[test]
    fn apply_knobs_ignores_undeclared_names() {
        let base = SpreadArbConfig::default();
        let mut p = StrategyParams::new();
        p.set("trend_max_entry_price", dec!(0.50));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.trend_max_entry_price, dec!(0.50));
        assert_eq!(out.trend_min_price, base.trend_min_price);
    }

    #[test]
    fn twin_runs_the_strategys_own_logic_and_exits_on_the_shared_policy() {
        let strat = SpreadArbBuiltin::new(TrendConfig::default(), SpreadArbConfig::default());
        let factory = strat.shadow_factory().expect("spread_arb is evolvable");
        assert_eq!(factory.strategy(), "spread_arb");
        let params = StrategyParams::from_knobs(&factory.knobs());
        let twin = factory.make(&params).expect("twin builds");
        let mut replay = TwinReplay::new(twin, &ExitConfig::default());

        let m = market();
        replay.on_round(std::slice::from_ref(&m), &[], 0);
        let mut now = confirm(&mut replay, &m, 0, 70);
        assert_eq!(
            replay.open_positions(),
            0,
            "above-threshold prices are not a dip"
        );

        // Dip while confirmed → the strategy's own entry fires.
        now += 1_000;
        let dip = book(0.43, 0.45);
        replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "t", &dip, 1, 870, now));
        assert_eq!(
            replay.open_positions(),
            1,
            "twin must enter via its own logic"
        );

        // Run-up → the shared exit policy takes profit.
        now += 1_000;
        let up = book(0.95, 0.97);
        replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "t", &up, 1, 860, now));
        assert_eq!(replay.open_positions(), 0);
        let metrics = Metrics::from_trades(&replay.windowed_trades(1800, now));
        assert_eq!(metrics.sample_count, 1);
        assert_eq!(metrics.wins, 1);
        assert!(
            metrics.total_pnl > Decimal::ZERO,
            "expected a profit, got {}",
            metrics.total_pnl
        );
    }

    #[test]
    fn a_twin_with_a_tighter_ceiling_skips_the_same_dip() {
        // This is the counterfactual that makes evolution meaningful: same tick
        // stream, one knob moved, a different decision — produced by the
        // strategy's own code path in both cases.
        let m = market();
        let dip = book(0.43, 0.45);
        let run = |cap: Decimal| -> usize {
            let base = SpreadArbConfig {
                trend_max_entry_price: cap,
                ..Default::default()
            };
            let strat = SpreadArbBuiltin::new(TrendConfig::default(), base);
            let factory = strat.shadow_factory().unwrap();
            let mut params = StrategyParams::from_knobs(&factory.knobs());
            params.set("trend_max_entry_price", cap);
            let twin = factory.make(&params).unwrap();
            let mut replay = TwinReplay::new(twin, &ExitConfig::default());
            replay.on_round(std::slice::from_ref(&m), &[], 0);
            let now = confirm(&mut replay, &m, 0, 70);
            replay.on_tick(&tick_ctx(
                std::slice::from_ref(&m),
                "t",
                &dip,
                1,
                870,
                now + 1_000,
            ));
            replay.open_positions()
        };
        assert_eq!(run(dec!(0.45)), 1, "mid*factor≈0.43 is inside a 0.45 cap");
        assert_eq!(run(dec!(0.40)), 0, "the same dip is outside a 0.40 cap");
    }
}
