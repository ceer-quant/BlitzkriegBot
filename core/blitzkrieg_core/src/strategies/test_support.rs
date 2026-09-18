//! **Test-only** hosted adapters.
//!
//! PR-B hard switch: the kernel ships ZERO strategies. Production binaries
//! register nothing; every strategy — the shipped examples included — arrives
//! through the C ABI v2 dlopen path. The kernel-side `spread_arb` /
//! `trend_follow` / `mean_reversion` wrappers were deleted with the builtins.
//!
//! The engine/service/evolution unit tests still need SOMETHING hosted to drive
//! candidate dispatch, gate exemptions and hot parameters without building a
//! cdylib and dlopening it per test. These adapters fill exactly that role, and
//! they exist only under `cfg(test)` — they are never compiled into the kernel,
//! never registered by `Core`, and the decision logic they run is the shared
//! `strategy-logic` crate (the same one the example cdylibs use), so they cannot
//! drift from the reference.

use super::shadow_twin::ShadowFactory;
use super::{EngineStrategy, GateExemptions, StrategyCtx};
use crate::model::OrderbookSnapshot;
use crate::shadow_evolution::{KnobSpec, ParamRegistry, StrategyParams};
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::sync::Arc;

// ── spread_arb (the trend-confirmed dip buyer) ──────────────────────────────

/// Test adapter over `strategy_logic::spread_arb`, mirroring what the shipped
/// `spread_arb_strategy` cdylib does through the C ABI.
pub struct TestSpreadArb {
    trend_cfg: TrendConfig,
    cfg: SpreadArbConfig,
    trend: strategy_logic::TrendTracker,
    hot_params: Option<Arc<ArcSwap<StrategyParams>>>,
}

impl TestSpreadArb {
    pub fn new(trend_cfg: TrendConfig, cfg: SpreadArbConfig) -> Self {
        Self {
            trend: strategy_logic::TrendTracker::new(trend_cfg.clone()),
            trend_cfg,
            cfg,
            hot_params: None,
        }
    }

    fn effective_cfg(&self) -> SpreadArbConfig {
        match &self.hot_params {
            Some(h) => strategy_logic::spread_arb_apply_knobs(&self.cfg, &h.load()),
            None => self.cfg.clone(),
        }
    }

    fn effective_trend(&self) -> TrendConfig {
        if let Some(h) = &self.hot_params {
            strategy_logic::spread_arb::apply_trend_knobs(&self.trend_cfg, &h.load())
        } else {
            self.trend_cfg.clone()
        }
    }

    fn sync_trend_cfg(&mut self) {
        let cfg = self.effective_trend();
        self.trend.set_config(cfg);
    }
}

impl EngineStrategy for TestSpreadArb {
    fn name(&self) -> &str {
        "spread_arb"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        self.sync_trend_cfg();
        self.trend.on_price(token_id, snap.mid_price, now_ms);
    }

    fn on_round(&mut self, slot: i64, _time_left_sec: i64, _now_ms: i64) {
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
            if let Some(sig) = strategy_logic::evaluate_spread_arb(
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
        let mut tokens: Vec<String> = self.trend.confirmed_tokens().into_iter().collect();
        tokens.sort();
        tokens
            .into_iter()
            .map(|t| {
                let mid = ctx
                    .fresh_book(&t)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                let entry = mid * eff.trend_entry_factor;
                let in_band = entry > Decimal::ZERO
                    && entry <= eff.trend_max_entry_price
                    && mid >= eff.trend_broken_price;
                serde_json::json!({
                    "token": t, "mid": mid, "entry": entry,
                    "cap": eff.trend_max_entry_price, "inBand": in_band,
                })
            })
            .collect()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        self.hot_params = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        strategy_logic::spread_arb_knobs(&self.effective_cfg())
    }

    fn config_view_json(&self) -> Option<String> {
        // Same shape the shipped `spread_arb_strategy` cdylib reports through
        // its `bk_strategy_config_view` symbol.
        let cfg = self.effective_cfg();
        Some(
            serde_json::json!({
                "trendMinPrice": cfg.trend_min_price.to_string(),
                "trendConfirmSec": cfg.trend_confirm_sec,
                "trendBrokenPrice": cfg.trend_broken_price.to_string(),
                "trendEntryPrice": cfg.trend_entry_price.to_string(),
                "trendEntryFactor": cfg.trend_entry_factor.to_string(),
                "trendMaxEntryPrice": cfg.trend_max_entry_price.to_string(),
            })
            .to_string(),
        )
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        Some(Box::new(TestSpreadArbFactory {
            base: self.cfg.clone(),
            trend: self.trend_cfg.clone(),
        }))
    }

    fn on_config(&mut self, trend: &TrendConfig, spread_arb: &SpreadArbConfig) {
        self.cfg = spread_arb.clone();
        self.trend_cfg = trend.clone();
        self.trend.set_config(trend.clone());
    }
}

pub struct TestSpreadArbFactory {
    pub base: SpreadArbConfig,
    pub trend: TrendConfig,
}

impl ShadowFactory for TestSpreadArbFactory {
    fn strategy(&self) -> String {
        "spread_arb".to_string()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        strategy_logic::spread_arb_knobs(&self.base)
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        let mut twin = TestSpreadArb::new(
            self.trend.clone(),
            strategy_logic::spread_arb_apply_knobs(&self.base, params),
        );
        // Pre-seed the twin's tracker with the counterfactual trend knobs, so a
        // variant of `min_price`/`broken_price` actually changes confirmation.
        let trend = strategy_logic::spread_arb::apply_trend_knobs(&self.trend, params);
        twin.trend.set_config(trend);
        Some(Box::new(twin))
    }
}

// ── trend_follow (the chase leg) ────────────────────────────────────────────

/// Test adapter over `strategy_logic::trend_follow`.
pub struct TestTrendFollow {
    cfg: strategy_logic::TrendFollowConfig,
    tracker: strategy_logic::MomentumTracker,
    hot_params: Option<Arc<ArcSwap<StrategyParams>>>,
}

impl TestTrendFollow {
    pub fn new(cfg: strategy_logic::TrendFollowConfig) -> Self {
        Self {
            tracker: strategy_logic::MomentumTracker::new(cfg.clone()),
            cfg,
            hot_params: None,
        }
    }

    fn effective_cfg(&self) -> strategy_logic::TrendFollowConfig {
        match &self.hot_params {
            Some(h) => strategy_logic::trend_follow::apply_knobs(&self.cfg, &h.load()),
            None => self.cfg.clone(),
        }
    }

    fn sync_tracker_cfg(&mut self) {
        let cfg = self.effective_cfg();
        self.tracker.set_config(cfg);
    }
}

impl EngineStrategy for TestTrendFollow {
    fn name(&self) -> &str {
        "trend_follow"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        self.sync_tracker_cfg();
        self.tracker.on_price(token_id, snap.mid_price, now_ms);
    }

    fn on_round(&mut self, slot: i64, _time_left_sec: i64, _now_ms: i64) {
        self.tracker.reset_if_new_round(slot);
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        self.tracker.take_broken()
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        self.tracker.confirmed_tokens()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        if self.tracker.confirmed_tokens().is_empty() {
            return Vec::new();
        }
        let cfg = self.effective_cfg();
        let now = ctx.now_ms();
        let mut out = Vec::new();
        for market in ctx.markets() {
            let up_book = ctx.fresh_book(&market.up_token_id);
            let down_book = ctx.fresh_book(&market.down_token_id);
            if let Some(sig) = strategy_logic::evaluate_trend_follow(
                &market.asset,
                &market.condition_id,
                &market.up_token_id,
                &market.down_token_id,
                up_book.as_ref(),
                down_book.as_ref(),
                &self.tracker,
                now,
                &cfg,
            ) {
                out.push(sig);
            }
        }
        out
    }

    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let eff = self.effective_cfg();
        let mut tokens: Vec<String> = self.tracker.confirmed_tokens().into_iter().collect();
        tokens.sort();
        tokens
            .into_iter()
            .map(|t| {
                let mid = ctx
                    .fresh_book(&t)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                let entry = ctx
                    .fresh_book(&t)
                    .map(|b| strategy_logic::round2(b.best_ask))
                    .unwrap_or(Decimal::ZERO);
                let chaseable =
                    entry > mid && entry <= eff.max_entry_price && mid >= eff.min_confirm_price;
                serde_json::json!({
                    "token": t, "mid": mid, "movePct": self.tracker.move_pct(&t, ctx.now_ms()),
                    "entry": entry, "cap": eff.max_entry_price, "chaseable": chaseable,
                })
            })
            .collect()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        self.hot_params = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        strategy_logic::trend_follow_knobs(&self.effective_cfg())
    }

    fn config_view_json(&self) -> Option<String> {
        let cfg = self.effective_cfg();
        Some(
            serde_json::json!({
                "momentumWindowSec": cfg.momentum_window_sec,
                "minMovePct": cfg.min_move_pct.to_string(),
                "minConfirmPrice": cfg.min_confirm_price.to_string(),
                "breakPrice": cfg.break_price.to_string(),
                "maxEntryPrice": cfg.max_entry_price.to_string(),
                "maxSpreadPct": cfg.max_spread_pct.to_string(),
            })
            .to_string(),
        )
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        Some(Box::new(TestTrendFollowFactory {
            base: self.cfg.clone(),
        }))
    }
}

pub struct TestTrendFollowFactory {
    base: strategy_logic::TrendFollowConfig,
}

impl ShadowFactory for TestTrendFollowFactory {
    fn strategy(&self) -> String {
        "trend_follow".to_string()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        strategy_logic::trend_follow_knobs(&self.base)
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        Some(Box::new(TestTrendFollow::new(
            strategy_logic::trend_follow::apply_knobs(&self.base, params),
        )))
    }
}

// ── mean_reversion (the fade leg) ───────────────────────────────────────────

/// Test adapter over `strategy_logic::mean_reversion`. Its one deliberate
/// declaration is the momentum-gate exemption (mirrors the shipped cdylib).
pub struct TestMeanReversion {
    cfg: strategy_logic::MeanReversionConfig,
    tracker: strategy_logic::FadeTracker,
    hot_params: Option<Arc<ArcSwap<StrategyParams>>>,
}

impl TestMeanReversion {
    pub fn new(cfg: strategy_logic::MeanReversionConfig) -> Self {
        Self {
            tracker: strategy_logic::FadeTracker::new(cfg.clone()),
            cfg,
            hot_params: None,
        }
    }

    fn effective_cfg(&self) -> strategy_logic::MeanReversionConfig {
        match &self.hot_params {
            Some(h) => strategy_logic::mean_reversion::apply_knobs(&self.cfg, &h.load()),
            None => self.cfg.clone(),
        }
    }

    fn sync_tracker_cfg(&mut self) {
        let cfg = self.effective_cfg();
        self.tracker.set_config(cfg);
    }
}

impl EngineStrategy for TestMeanReversion {
    fn name(&self) -> &str {
        "mean_reversion"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        self.sync_tracker_cfg();
        self.tracker.on_price(token_id, snap.mid_price, now_ms);
    }

    fn on_round(&mut self, slot: i64, _time_left_sec: i64, _now_ms: i64) {
        self.tracker.reset_if_new_round(slot);
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        self.tracker.take_broken()
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        self.tracker.zone_tokens()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let cfg = self.effective_cfg();
        let now = ctx.now_ms();
        let mut out = Vec::new();
        for market in ctx.markets() {
            let up_book = ctx.fresh_book(&market.up_token_id);
            let down_book = ctx.fresh_book(&market.down_token_id);
            for (token, book) in [
                (market.up_token_id.clone(), up_book.as_ref()),
                (market.down_token_id.clone(), down_book.as_ref()),
            ] {
                if book.is_none() || !self.tracker.is_in_zone(&token) {
                    continue;
                }
                if !self.tracker.try_fire(&token, now) {
                    continue;
                }
                if let Some(sig) = strategy_logic::evaluate_mean_reversion(
                    &market.asset,
                    &market.condition_id,
                    &market.up_token_id,
                    &market.down_token_id,
                    if token == market.up_token_id {
                        book
                    } else {
                        None
                    },
                    if token == market.down_token_id {
                        book
                    } else {
                        None
                    },
                    &self.tracker,
                    now,
                    &cfg,
                ) {
                    out.push(sig);
                }
            }
        }
        out
    }

    fn gate_exemptions(&self) -> GateExemptions {
        GateExemptions {
            timing: false,
            momentum: true,
        }
    }

    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let eff = self.effective_cfg();
        let mut tokens: Vec<String> = self.tracker.zone_tokens().into_iter().collect();
        tokens.sort();
        tokens
            .into_iter()
            .map(|t| {
                let mid = ctx
                    .fresh_book(&t)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                let drop = self.tracker.drop_pct(&t, ctx.now_ms());
                let entry = mid * eff.entry_factor;
                let firable = mid > Decimal::ZERO
                    && mid <= eff.max_price
                    && drop <= -eff.min_drop_pct
                    && entry < mid;
                serde_json::json!({
                    "token": t, "mid": mid, "dropPct": drop,
                    "entry": entry, "cap": eff.max_price, "firable": firable,
                })
            })
            .collect()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        self.hot_params = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        strategy_logic::mean_reversion_knobs(&self.effective_cfg())
    }

    fn config_view_json(&self) -> Option<String> {
        let cfg = self.effective_cfg();
        Some(
            serde_json::json!({
                "lookbackSec": cfg.lookback_sec,
                "minDropPct": cfg.min_drop_pct.to_string(),
                "maxPrice": cfg.max_price.to_string(),
                "entryFactor": cfg.entry_factor.to_string(),
                "maxSpreadPct": cfg.max_spread_pct.to_string(),
                "cooldownSec": cfg.cooldown_sec,
            })
            .to_string(),
        )
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        Some(Box::new(TestMeanReversionFactory {
            base: self.cfg.clone(),
        }))
    }
}

pub struct TestMeanReversionFactory {
    base: strategy_logic::MeanReversionConfig,
}

impl ShadowFactory for TestMeanReversionFactory {
    fn strategy(&self) -> String {
        "mean_reversion".to_string()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        strategy_logic::mean_reversion_knobs(&self.base)
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        Some(Box::new(TestMeanReversion::new(
            strategy_logic::mean_reversion::apply_knobs(&self.base, params),
        )))
    }
}

/// Host all three adapters into an engine with `spread_arb` enabled — the
/// registration shape the pre-PR-B builtins had, so dispatch/gate tests keep
/// their meaning. Source is tagged `test` (never `builtin`: the kernel has none).
pub fn host(
    engine: &mut super::super::engine::Engine,
    trend: TrendConfig,
    spread_arb: SpreadArbConfig,
) {
    engine
        .register_user_strategy(
            Box::new(TestSpreadArb::new(trend, spread_arb)),
            "test".into(),
        )
        .expect("fresh engine");
    engine
        .register_user_strategy(
            Box::new(TestTrendFollow::new(Default::default())),
            "test".into(),
        )
        .expect("fresh engine");
    engine
        .register_user_strategy(
            Box::new(TestMeanReversion::new(Default::default())),
            "test".into(),
        )
        .expect("fresh engine");
    assert!(engine.set_strategy_enabled("spread_arb", true));
}
