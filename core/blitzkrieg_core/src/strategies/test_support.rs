//! **Test-only** hosted adapters.
//!
//! PR-B hard switch: the kernel ships ZERO strategies. Production binaries
//! register nothing; every strategy arrives through the C ABI v2 dlopen path.
//! The kernel-side `spread_arb` / `trend_follow` / `mean_reversion` wrappers were
//! deleted with the builtins, and the example cdylibs that used to live in
//! `user_layer/strategies/` are gone too — the reference implementation is now
//! `user_layer/parity_strategy`.
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
        // its `bk_strategy_config_view` symbol, HFT entry filters included.
        let cfg = self.effective_cfg();
        Some(
            serde_json::json!({
                "trendMinPrice": cfg.trend_min_price.to_string(),
                "trendConfirmSec": cfg.trend_confirm_sec,
                "trendBrokenPrice": cfg.trend_broken_price.to_string(),
                "trendEntryPrice": cfg.trend_entry_price.to_string(),
                "trendEntryFactor": cfg.trend_entry_factor.to_string(),
                "trendMaxEntryPrice": cfg.trend_max_entry_price.to_string(),
                "entryMinObi": cfg.entry_min_obi.to_string(),
                "entryMaxSpreadPct": cfg.entry_max_spread_pct.to_string(),
                "entryDipMaxPct": cfg.entry_dip_max_pct.to_string(),
                "entryBounceMinPct": cfg.entry_bounce_min_pct.to_string(),
                "entryBounceWindowSec": cfg.entry_bounce_window_sec,
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

    fn on_config(&mut self, config: &serde_json::Value) {
        // RFC-645 A-2: deserialize this strategy's OWN knobs out of the ONE
        // opaque-JSON push. A parse failure is the config-REJECTION path:
        // advisory warn, the previous config stays in force, never a panic —
        // the same posture the C ABI's non-zero reject has always had.
        match serde_json::from_value::<StrategyConfigJson>(config.clone()) {
            Ok(cfg) => {
                if let Some(trend) = cfg.trend {
                    self.trend_cfg = trend.into();
                    self.trend.set_config(self.trend_cfg.clone());
                }
                if let Some(spread_arb) = cfg.spread_arb {
                    self.cfg = spread_arb.into();
                }
            }
            Err(e) => {
                tracing::warn!(
                    strategy = "spread_arb",
                    error = %e,
                    "config push rejected (malformed projection) — keeping the previous config"
                );
            }
        }
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
            timing_min_time_left_sec: None,
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
                let trend_drop = self.tracker.trend_drop_pct(&t, ctx.now_ms());
                let entry = mid * eff.entry_factor;
                // #176: the trend gate is part of "would the evaluator place?".
                let in_slide = self.tracker.in_trend_slide(&t, ctx.now_ms());
                let firable = mid > Decimal::ZERO
                    && mid <= eff.max_price
                    && drop <= -eff.min_drop_pct
                    && !in_slide
                    && entry < mid;
                serde_json::json!({
                    "token": t, "mid": mid, "dropPct": drop,
                    "trendDropPct": trend_drop, "inTrendSlide": in_slide,
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
                // #176 trend gate: 0 = off (pre-gate behaviour).
                "trendWindowSec": cfg.trend_window_sec,
                "trendDropPct": cfg.trend_drop_pct.to_string(),
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
    host_all(engine, trend, spread_arb);
    assert!(engine.set_strategy_enabled("spread_arb", true));
}

/// Host all three adapters with every one DISABLED — the zero-default shape a
/// fresh boot has, because the kernel enables nothing by itself. Tests that
/// want a specific strategy trading opt in explicitly (the same way an
/// operator does), which keeps this helper honest about the default.
pub fn host_disabled(
    engine: &mut super::super::engine::Engine,
    trend: TrendConfig,
    spread_arb: SpreadArbConfig,
) {
    host_all(engine, trend, spread_arb);
}

fn host_all(
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
}

// ── RFC-645 A-2: the builtin adapter's config deserialization face ──────────

/// The wire shape a builtin strategy deserializes out of the ONE opaque-JSON
/// config push ([`super::EngineStrategy::on_config`]). Mirrors the kernel's
/// [`crate::engine::engine_strategy_config_json`] projection field-for-field:
/// camelCase keys, decimals as STRINGS, ints as JSON numbers. Deserialization
/// failures are advisory (the caller keeps its previous config), so the
/// default impls only ever fire on a malformed push.
#[derive(Debug, serde::Deserialize)]
struct StrategyConfigJson {
    #[serde(default)]
    trend: Option<TrendJson>,
    #[serde(default, rename = "spreadArb")]
    spread_arb: Option<SpreadArbJson>,
}

/// A field-presence-tolerant read: a push missing a whole section leaves that
/// section's previous config untouched (a partial projection is not an error).
#[derive(Debug, serde::Deserialize)]
struct TrendJson {
    #[serde(default = "default_confirm_sec", rename = "confirmSec")]
    confirm_sec: i64,
    #[serde(default = "default_min_price", rename = "minPrice")]
    min_price: Decimal,
    #[serde(default = "default_broken_price", rename = "brokenPrice")]
    broken_price: Decimal,
    #[serde(default = "default_ratio", rename = "ratio")]
    ratio: Decimal,
    #[serde(default = "default_window_floor_ms", rename = "windowFloorMs")]
    window_floor_ms: i64,
}

fn default_confirm_sec() -> i64 {
    TrendConfig::default().confirm_sec
}
fn default_min_price() -> Decimal {
    TrendConfig::default().min_price
}
fn default_broken_price() -> Decimal {
    TrendConfig::default().broken_price
}
fn default_ratio() -> Decimal {
    TrendConfig::default().ratio
}
fn default_window_floor_ms() -> i64 {
    TrendConfig::default().window_floor_ms
}

impl From<TrendJson> for TrendConfig {
    fn from(j: TrendJson) -> Self {
        Self {
            confirm_sec: j.confirm_sec,
            min_price: j.min_price,
            broken_price: j.broken_price,
            ratio: j.ratio,
            window_floor_ms: j.window_floor_ms,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct SpreadArbJson {
    #[serde(default = "default_trend_min_price", rename = "trendMinPrice")]
    trend_min_price: Decimal,
    #[serde(default = "default_trend_confirm_sec", rename = "trendConfirmSec")]
    trend_confirm_sec: i64,
    #[serde(default = "default_trend_broken_price", rename = "trendBrokenPrice")]
    trend_broken_price: Decimal,
    #[serde(default = "default_trend_entry_price", rename = "trendEntryPrice")]
    trend_entry_price: Decimal,
    #[serde(default = "default_trend_entry_factor", rename = "trendEntryFactor")]
    trend_entry_factor: Decimal,
    #[serde(
        default = "default_trend_max_entry_price",
        rename = "trendMaxEntryPrice"
    )]
    trend_max_entry_price: Decimal,
    #[serde(default = "default_entry_min_obi", rename = "entryMinObi")]
    entry_min_obi: Decimal,
    #[serde(default = "default_entry_max_spread_pct", rename = "entryMaxSpreadPct")]
    entry_max_spread_pct: Decimal,
    #[serde(default = "default_entry_dip_max_pct", rename = "entryDipMaxPct")]
    entry_dip_max_pct: Decimal,
    #[serde(default = "default_entry_bounce_min_pct", rename = "entryBounceMinPct")]
    entry_bounce_min_pct: Decimal,
    #[serde(
        default = "default_entry_bounce_window_sec",
        rename = "entryBounceWindowSec"
    )]
    entry_bounce_window_sec: i64,
}

fn default_trend_min_price() -> Decimal {
    SpreadArbConfig::default().trend_min_price
}
fn default_trend_confirm_sec() -> i64 {
    SpreadArbConfig::default().trend_confirm_sec
}
fn default_trend_broken_price() -> Decimal {
    SpreadArbConfig::default().trend_broken_price
}
fn default_trend_entry_price() -> Decimal {
    SpreadArbConfig::default().trend_entry_price
}
fn default_trend_entry_factor() -> Decimal {
    SpreadArbConfig::default().trend_entry_factor
}
fn default_trend_max_entry_price() -> Decimal {
    SpreadArbConfig::default().trend_max_entry_price
}
fn default_entry_min_obi() -> Decimal {
    SpreadArbConfig::default().entry_min_obi
}
fn default_entry_max_spread_pct() -> Decimal {
    SpreadArbConfig::default().entry_max_spread_pct
}
fn default_entry_dip_max_pct() -> Decimal {
    SpreadArbConfig::default().entry_dip_max_pct
}
fn default_entry_bounce_min_pct() -> Decimal {
    SpreadArbConfig::default().entry_bounce_min_pct
}
fn default_entry_bounce_window_sec() -> i64 {
    SpreadArbConfig::default().entry_bounce_window_sec
}

impl From<SpreadArbJson> for SpreadArbConfig {
    fn from(j: SpreadArbJson) -> Self {
        Self {
            trend_min_price: j.trend_min_price,
            trend_confirm_sec: j.trend_confirm_sec,
            trend_broken_price: j.trend_broken_price,
            trend_entry_price: j.trend_entry_price,
            trend_entry_factor: j.trend_entry_factor,
            trend_max_entry_price: j.trend_max_entry_price,
            entry_min_obi: j.entry_min_obi,
            entry_max_spread_pct: j.entry_max_spread_pct,
            entry_dip_max_pct: j.entry_dip_max_pct,
            entry_bounce_min_pct: j.entry_bounce_min_pct,
            entry_bounce_window_sec: j.entry_bounce_window_sec,
        }
    }
}

// ── RFC-645 A-2 golden: the projection IS the foreign marshal ───────────────

#[cfg(test)]
mod rfc645_config_push_tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// THE byte-level pin (RFC-645 决议④): the kernel's
    /// [`crate::engine::engine_strategy_config_json`] projection must be
    /// byte-for-byte what `ForeignStrategy::on_config` (strategies/foreign.rs)
    /// marshalled to dylibs for the whole life of the v2 ABI — camelCase keys
    /// in exactly this order, every Decimal rendered as a STRING, ints as JSON
    /// numbers. Drift here is a WIRE BREAK (every shipped cdylib parses these
    /// bytes), not a refactor. If this test goes red, the projection function
    /// was changed and the dylib contract moved with it — restore the
    /// spelling.
    #[test]
    fn engine_strategy_config_json_is_the_foreign_marshal_golden() {
        let trend = TrendConfig {
            confirm_sec: 90,
            min_price: dec!(0.55),
            broken_price: dec!(0.35),
            ratio: dec!(0.80),
            window_floor_ms: 10_000,
        };
        let spread_arb = SpreadArbConfig {
            trend_min_price: dec!(0.55),
            trend_confirm_sec: 90,
            trend_broken_price: dec!(0.35),
            trend_entry_price: dec!(0.50),
            trend_entry_factor: dec!(0.88),
            trend_max_entry_price: dec!(0.45),
            entry_min_obi: dec!(-0.10),
            entry_max_spread_pct: dec!(2.5),
            entry_dip_max_pct: dec!(6),
            entry_bounce_min_pct: dec!(0.4),
            entry_bounce_window_sec: 7,
        };
        let wire = crate::engine::engine_strategy_config_json(&trend, &spread_arb).to_string();
        // The full expected wire shape, verbatim. Key ORDER is alphabetical:
        // serde_json's default build serializes `Value::Object` through its
        // BTreeMap-backed Map, so both the OLD `json!` marshal in foreign.rs
        // and the new projection function have ALWAYS emitted sorted keys —
        // the wire contract below is what every shipped cdylib has parsed all
        // along. Decimals are quoted strings (the ABI's numeric form), i64
        // knobs are bare JSON numbers.
        let golden = concat!(
            r#"{"spreadArb":{"entryBounceMinPct":"0.4","entryBounceWindowSec":7,"#,
            r#""entryDipMaxPct":"6","entryMaxSpreadPct":"2.5","entryMinObi":"-0.10","#,
            r#""trendBrokenPrice":"0.35","trendConfirmSec":90,"trendEntryFactor":"0.88","#,
            r#""trendEntryPrice":"0.50","trendMaxEntryPrice":"0.45","trendMinPrice":"0.55"},"#,
            r#""trend":{"brokenPrice":"0.35","confirmSec":90,"minPrice":"0.55","#,
            r#""ratio":"0.80","windowFloorMs":10000}}"#
        );
        assert_eq!(
            wire, golden,
            "the strategy-config JSON projection drifted from the foreign marshal wire contract"
        );
        // Defaults ride the same shape (the zero-override boot config is what
        // every fresh scope is born with).
        let default_wire = crate::engine::engine_strategy_config_json(
            &TrendConfig::default(),
            &SpreadArbConfig::default(),
        )
        .to_string();
        let default_golden = concat!(
            r#"{"spreadArb":{"entryBounceMinPct":"0","entryBounceWindowSec":5,"#,
            r#""entryDipMaxPct":"0","entryMaxSpreadPct":"0","entryMinObi":"0","#,
            r#""trendBrokenPrice":"0.35","trendConfirmSec":60,"trendEntryFactor":"0.88","#,
            r#""trendEntryPrice":"0","trendMaxEntryPrice":"0.45","trendMinPrice":"0.55"},"#,
            r#""trend":{"brokenPrice":"0.35","confirmSec":60,"minPrice":"0.55","#,
            r#""ratio":"0.8","windowFloorMs":10000}}"#
        );
        assert_eq!(
            default_wire, default_golden,
            "the default-config projection drifted from the foreign marshal wire contract"
        );
    }

    /// The builtin adapter's reject path: a push that does not parse is
    /// ADVISORY — the previous config stays in force, the adapter warns,
    /// nothing panics.
    #[test]
    fn on_config_parse_failure_keeps_the_previous_config_advisory() {
        let mut s = TestSpreadArb::new(TrendConfig::default(), SpreadArbConfig::default());
        let before = s.cfg.clone();
        let before_trend = s.trend_cfg.clone();
        // A structurally-wrong push: a spreadArb field carries a WRONG type
        // (not the decimal string the wire defines) — the whole spreadArb
        // section fails to deserialize and must be refused as a unit.
        let malformed = serde_json::json!({
            "trend": { "confirmSec": 90, "minPrice": "0.55", "brokenPrice": "0.35",
                       "ratio": "0.8", "windowFloorMs": 10000 },
            "spreadArb": { "trendMinPrice": "not-a-decimal", "trendConfirmSec": 90 }
        });
        s.on_config(&malformed);
        assert_eq!(
            s.cfg, before,
            "a malformed section is rejected WHOLE: the previous config stays"
        );
        // The well-formed trend section beside a malformed sibling is ALSO not
        // applied: the projection is one document, one verdict (reject = keep
        // everything), which is what makes "keep the previous config" a
        // meaningful invariant.
        assert_eq!(
            s.trend_cfg, before_trend,
            "no partial application: one malformed section rejects the whole push"
        );
    }

    /// The happy path round-trip: a projection built from one config, fed
    /// back through the adapter's deserializer, reproduces that config
    /// exactly — the same values the direct `&TrendConfig/&SpreadArbConfig`
    /// push used to carry (the behaviour-equivalence clause of the RFC).
    #[test]
    fn on_config_projection_round_trip_reproduces_the_config() {
        let trend = TrendConfig {
            confirm_sec: 45,
            window_floor_ms: 2_000,
            ..TrendConfig::default()
        };
        let spread_arb = SpreadArbConfig {
            trend_entry_factor: dec!(0.8),
            entry_min_obi: dec!(-0.1),
            entry_bounce_window_sec: 7,
            ..SpreadArbConfig::default()
        };
        let mut s = TestSpreadArb::new(TrendConfig::default(), SpreadArbConfig::default());
        s.on_config(&crate::engine::engine_strategy_config_json(
            &trend,
            &spread_arb,
        ));
        assert_eq!(
            s.trend_cfg, trend,
            "trend knobs survive the JSON round-trip"
        );
        assert_eq!(
            s.cfg, spread_arb,
            "spread_arb knobs survive the JSON round-trip"
        );
        // The live tracker picked the new knobs up too (the pre-RFC
        // behaviour: on_config re-seeds the tracker's own config copy).
        let mut tracker_check =
            TestSpreadArb::new(TrendConfig::default(), SpreadArbConfig::default());
        tracker_check.on_config(&crate::engine::engine_strategy_config_json(
            &trend,
            &spread_arb,
        ));
        tracker_check.sync_trend_cfg();
        // `sync_trend_cfg` reads back through effective_trend (hot-params
        // aware): no registry is attached, so it must now serve the pushed
        // trend config — the observable "the new knobs are live" verdict.
        assert_eq!(
            tracker_check.effective_trend(),
            trend,
            "the tracker serves the pushed config after the push"
        );
    }
}
