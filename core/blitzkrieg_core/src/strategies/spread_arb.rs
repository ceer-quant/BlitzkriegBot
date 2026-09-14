//! The built-in `spread_arb` strategy, moved behind the [`EngineStrategy`]
//! seam in P-1.1.
//!
//! Behaviour is unchanged from when this logic lived directly in `engine.rs`:
//!  - it owns the trend tracker (confirmation window, broken-price detection)
//!  - `find_candidates` mirrors `evaluate_spread_arb` over the round's markets
//!  - Shadow Evolution's mutable parameters overlay the base config through the
//!    hot-swap handle, read fresh on every evaluation.

use super::{EngineStrategy, StrategyCtx};
use crate::model::OrderbookSnapshot;
use crate::shadow_evolution::MutableParams;
use crate::signal::{
    evaluate_spread_arb, SpreadArbConfig, TradeSignal, TrendConfig, TrendTracker,
};
use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::sync::Arc;

pub struct SpreadArbBuiltin {
    cfg: SpreadArbConfig,
    trend: TrendTracker,
    hot_params: Option<Arc<ArcSwap<MutableParams>>>,
}

impl SpreadArbBuiltin {
    pub fn new(trend_cfg: TrendConfig, cfg: SpreadArbConfig) -> Self {
        Self { cfg, trend: TrendTracker::new(trend_cfg), hot_params: None }
    }

    /// The spread_arb config currently in force: base config overlaid with the
    /// hot-swapped mutable parameters when Shadow Evolution is active.
    fn effective_cfg(&self) -> SpreadArbConfig {
        let mut cfg = self.cfg.clone();
        if let Some(h) = &self.hot_params {
            let p = h.load();
            cfg.trend_min_price = p.trend_min_price;
            cfg.trend_entry_factor = p.trend_entry_factor;
            cfg.trend_max_entry_price = p.trend_max_entry_price;
            cfg.trend_broken_price = p.trend_broken_price;
        }
        cfg
    }
}

impl EngineStrategy for SpreadArbBuiltin {
    fn name(&self) -> &str {
        "spread_arb"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
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

    fn set_hot_params(&mut self, handle: Arc<ArcSwap<MutableParams>>) {
        self.hot_params = Some(handle);
    }

    fn spread_arb_view(&self) -> Option<SpreadArbConfig> {
        Some(self.effective_cfg())
    }

    fn on_config(&mut self, trend: &TrendConfig, spread_arb: &SpreadArbConfig) {
        self.cfg = spread_arb.clone();
        self.trend.set_config(trend.clone());
    }
}
