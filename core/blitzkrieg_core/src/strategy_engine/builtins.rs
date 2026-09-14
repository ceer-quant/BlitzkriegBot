//! Built-in strategies — adapter from the existing evaluators to the `Strategy`
//! trait, so the kernel's strategy engine can run the proven `spread_arb` logic
//! alongside user-layer dynamic strategies. No behaviour change: this wraps the
//! existing pure evaluator, it does not reimplement it.

use super::{MarketTick, Signal, Strategy};
use crate::signal::{SpreadArbConfig, TrendConfig, TrendTracker};
use rust_decimal::Decimal;

/// Trend-confirmed dip buyer, exposed through the `Strategy` trait. Uses the
/// engine's own trend tracker so it is self-contained when loaded dynamically.
pub struct SpreadArbStrategy {
    trend: TrendTracker,
    cfg: SpreadArbConfig,
    min_confirm_samples: usize,
    samples: std::collections::HashMap<String, usize>,
}

impl SpreadArbStrategy {
    pub fn new(trend_cfg: TrendConfig, cfg: SpreadArbConfig) -> Self {
        Self {
            trend: TrendTracker::new(trend_cfg),
            cfg,
            min_confirm_samples: 1,
            samples: Default::default(),
        }
    }
}

impl Strategy for SpreadArbStrategy {
    fn name(&self) -> &str {
        "spread_arb"
    }

    fn on_tick(&mut self, tick: &MarketTick) -> Option<Signal> {
        // Maintain trend state; only propose an entry once confirmed and the mid
        // has pulled back under the entry cap (mirrors evaluate_spread_arb).
        self.trend.on_price(&tick.symbol, tick.mid, tick.timestamp_ms);
        let n = self.samples.entry(tick.symbol.clone()).or_insert(0);
        *n += 1;
        if *n < self.min_confirm_samples {
            return None;
        }
        if !self.trend.is_confirmed(&tick.symbol) {
            return None;
        }
        if tick.mid < self.cfg.trend_broken_price {
            return None;
        }
        let entry = (tick.mid * self.cfg.trend_entry_factor).min(tick.best_bid).min(self.cfg.trend_max_entry_price);
        if entry <= Decimal::ZERO || entry >= tick.mid {
            return None;
        }
        Some(Signal::Buy { symbol: tick.symbol.clone(), price: entry, size: Decimal::from(10) })
    }

    fn on_round(&mut self, slot: i64) {
        self.trend.reset_if_new_round(slot);
        self.samples.clear();
    }
}
