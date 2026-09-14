//! Shadow Evolution — variants: virtual strategies that replay the same ticks
//! with mutated parameters, using a virtual ledger (zero real cost).
//!
//! Each variant runs the SAME entry criteria (`spread_arb`-style) and the SAME
//! exit policy (`exit_policy`, shared with live) as the kernel — only the mutable
//! parameters differ. That makes the comparison a true counterfactual: identical
//! data, identical machinery, different knobs.

use super::config::{ImmutableConfig, MutableParams};
use crate::exit_policy::{
    decide_exit, executable_bid, taker_fee_pct, update_exit_state, ExitConfig, ExitState,
    ExitTickInput,
};
use crate::model::OrderbookSnapshot;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};

/// A simulated open position for one variant.
#[derive(Debug, Clone)]
struct VirtualPosition {
    #[allow(dead_code)]
    token_id: String,
    entry_price: Decimal,
    shares: Decimal,
    expires_at_ms: i64,
    state: ExitState,
}

/// Closed virtual trade (for windowed metrics).
#[derive(Debug, Clone)]
struct VirtualTrade {
    exit_ms: i64,
    net_pnl: Decimal,
}

pub struct Variant {
    pub id: String,
    pub label: String,
    pub params: MutableParams,
    pub is_baseline: bool,
    pub created_at_ms: i64,
    pub crashed: bool,
    open: HashMap<String, VirtualPosition>,
    trades: VecDeque<VirtualTrade>,
    /// Exit config used by the simulation (derived from immutable risk).
    exit_cfg: ExitConfig,
}

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
}

impl Variant {
    pub fn new(id: String, label: String, params: MutableParams, is_baseline: bool, now_ms: i64, risk: &ImmutableConfig) -> Self {
        // Simulated exits use the immutable hard stop (never loosened) plus the
        // shared exit-policy defaults for TP/trailing.
        let mut exit_cfg = ExitConfig::default();
        exit_cfg.stop_loss_pct = risk.hard_stop_loss_pct;
        Self {
            id,
            label,
            params,
            is_baseline,
            created_at_ms: now_ms,
            crashed: false,
            open: HashMap::new(),
            trades: VecDeque::new(),
            exit_cfg,
        }
    }

    pub fn open_positions(&self) -> usize {
        self.open.len()
    }

    /// Feed one market tick. `confirmed` is the shared trend-confirmation answer
    /// for this token (computed once by the caller from the real trend tracker).
    pub fn on_tick(
        &mut self,
        token_id: &str,
        book: &OrderbookSnapshot,
        confirmed: bool,
        expires_at_ms: i64,
        now_ms: i64,
    ) {
        // 1) Manage an existing virtual position.
        if let Some(pos) = self.open.get_mut(token_id) {
            update_exit_state(&mut pos.state, pos.entry_price, Some(book), now_ms, &self.exit_cfg);
            let time_left = (pos.expires_at_ms - now_ms) / 1000;
            let hold = (now_ms - (expires_at_ms - 900_000)).max(0) / 1000;
            if let Some(d) = decide_exit(ExitTickInput {
                entry_price: pos.entry_price,
                book: Some(book),
                fallback_price: Some(book.mid_price),
                time_left_sec: time_left,
                hold_sec: hold,
                state: &pos.state,
                now_ms,
                cfg: &self.exit_cfg,
            }) {
                let exit_price = executable_bid(Some(book));
                let shares = pos.shares;
                let entry = pos.entry_price;
                let gross = (exit_price - entry) * shares;
                let fee = taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
                let net = gross - fee;
                self.trades.push_back(VirtualTrade { exit_ms: now_ms, net_pnl: net });
                let _ = d;
                self.open.remove(token_id);
            }
            return;
        }

        // 2) Consider an entry (mirrors evaluate_spread_arb's discipline).
        if !confirmed {
            return;
        }
        let mid = book.mid_price;
        if mid <= Decimal::ZERO || book.bids.is_empty() || book.asks.is_empty() {
            return;
        }
        let p = &self.params;
        if p.trend_broken_price > Decimal::ZERO && mid < p.trend_broken_price {
            return;
        }
        let raw = mid * p.trend_entry_factor;
        let mut entry = raw.max(Decimal::new(5, 2)).min(Decimal::new(9, 1));
        entry = (entry * Decimal::ONE_HUNDRED).round() / Decimal::ONE_HUNDRED;
        if book.best_bid > Decimal::ZERO && entry > book.best_bid {
            entry = (book.best_bid * Decimal::ONE_HUNDRED).round() / Decimal::ONE_HUNDRED;
        }
        if entry >= mid {
            return;
        }
        if p.trend_max_entry_price > Decimal::ZERO && entry > p.trend_max_entry_price {
            return;
        }

        let shares = Decimal::from(10);
        self.open.insert(
            token_id.to_string(),
            VirtualPosition {
                token_id: token_id.to_string(),
                entry_price: entry,
                shares,
                expires_at_ms,
                state: ExitState::new(entry, now_ms),
            },
        );
    }

    /// Metrics over the last `window_secs` (prunes older trades).
    pub fn metrics(&mut self, window_secs: i64, now_ms: i64) -> Metrics {
        let cutoff = now_ms - window_secs * 1000;
        while let Some(front) = self.trades.front() {
            if front.exit_ms < cutoff {
                self.trades.pop_front();
            } else {
                break;
            }
        }
        let mut m = Metrics::default();
        for t in &self.trades {
            m.sample_count += 1;
            m.total_pnl += t.net_pnl;
            if t.net_pnl > Decimal::ZERO {
                m.wins += 1;
                m.gross_profit += t.net_pnl;
            } else {
                m.gross_loss += -t.net_pnl;
            }
        }
        m
    }

    pub fn age_sec(&self, now_ms: i64) -> i64 {
        (now_ms - self.created_at_ms) / 1000
    }
}

/// Build the variant set from a base parameter set: the baseline (current live
/// params, `is_baseline=true`) plus `count-1` mutated variants. Mutations are
/// deterministic (no RNG) so behaviour is reproducible and testable.
pub fn build_variants(
    base: &MutableParams,
    count: usize,
    max_gradient: Decimal,
    risk: &ImmutableConfig,
    now_ms: i64,
) -> Vec<Variant> {
    let mut out = Vec::with_capacity(count.max(1));
    out.push(Variant::new("baseline".into(), "live(baseline)".into(), base.clone(), true, now_ms, risk));

    // Step sizes spread within the gradient bound; alternate direction per field
    // so the variants explore both sides without ever exceeding the lock.
    let steps = [Decimal::new(102, 2), Decimal::new(98, 2), Decimal::new(104, 2)];
    for i in 1..count {
        let f = steps.get((i - 1) % steps.len()).copied().unwrap_or(Decimal::ONE);
        let mut params = base.scaled(f);
        // Occasionally tighten the entry cap specifically (the most impactful knob).
        if i % 3 == 0 {
            params.trend_max_entry_price = base.trend_max_entry_price * Decimal::new(103, 2);
        }
        // Clamp every field to the gradient bound relative to the base.
        params = super::guard::clamped_step(base, &params, max_gradient);
        out.push(Variant::new(
            format!("variant-{i}"),
            format!("variant-{i}(x{f})"),
            params,
            false,
            now_ms,
            risk,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        use rust_decimal::prelude::FromPrimitive;
        OrderbookSnapshot::from_levels(
            "t",
            vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
            vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
            0,
        )
    }

    #[test]
    fn variant_enters_on_confirmed_dip_and_exits_on_profit() {
        let risk = ImmutableConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &risk);
        // Dip to 0.44 while confirmed → opens at ~0.43.
        v.on_tick("t", &book(0.43, 0.45), true, 900_000, 1000);
        assert_eq!(v.open_positions(), 1);
        // Price runs up → exit on profit (>100% TP or trailing).
        v.on_tick("t", &book(0.95, 0.97), true, 900_000, 2000);
        assert_eq!(v.open_positions(), 0);
        let m = v.metrics(1800, 2000);
        assert_eq!(m.sample_count, 1);
        assert!(m.total_pnl > Decimal::ZERO);
        assert_eq!(m.wins, 1);
    }

    #[test]
    fn unconfirmed_tick_does_not_enter() {
        let risk = ImmutableConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &risk);
        v.on_tick("t", &book(0.43, 0.45), false, 900_000, 1000);
        assert_eq!(v.open_positions(), 0);
    }

    #[test]
    fn metrics_prune_outside_the_window() {
        let risk = ImmutableConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &risk);
        v.on_tick("t", &book(0.43, 0.45), true, 900_000, 0);
        v.on_tick("t", &book(0.95, 0.97), true, 900_000, 1_000);
        assert_eq!(v.metrics(1800, 1_000).sample_count, 1);
        // Far in the future, the trade falls outside a 30-min window.
        assert_eq!(v.metrics(1800, 10_000_000).sample_count, 0);
    }

    #[test]
    fn variants_are_within_gradient_and_deterministic() {
        let base = MutableParams::default();
        let risk = ImmutableConfig::default();
        let vs = build_variants(&base, 3, dec!(0.05), &risk, 0);
        assert_eq!(vs.len(), 3);
        assert!(vs[0].is_baseline);
        for v in vs.iter().skip(1) {
            assert!(super::super::guard::validate_gradient(&base, &v.params, dec!(0.05)).is_ok());
        }
        let vs2 = build_variants(&base, 3, dec!(0.05), &risk, 0);
        assert_eq!(vs[1].params, vs2[1].params); // deterministic
    }
}
