//! Shadow Evolution — variants: virtual strategies that replay the same ticks
//! with mutated parameters, using a virtual ledger (zero real cost).
//!
//! Each variant runs the SAME entry criteria (`spread_arb`-style) and the SAME
//! exit policy (`exit_policy`, shared with live) as the kernel — only the mutable
//! parameters differ. That makes the comparison a true counterfactual: identical
//! data, identical machinery, different knobs.

use super::config::MutableParams;
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
    /// Exit policy replayed for this variant. Identical to the live position
    /// manager's (`PositionConfig.exit`), so the comparison is a true
    /// counterfactual where ONLY the mutable entry knobs differ (D-2).
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
    pub fn new(
        id: String,
        label: String,
        params: MutableParams,
        is_baseline: bool,
        now_ms: i64,
        exit_cfg: &ExitConfig,
    ) -> Self {
        // The variant replays the EXACT live exit policy (D-2). It is never
        // loosened to a fabricated hard stop: any difference in outcome must come
        // from the mutable entry knobs, not from a different exit mechanism.
        Self {
            id,
            label,
            params,
            is_baseline,
            created_at_ms: now_ms,
            crashed: false,
            open: HashMap::new(),
            trades: VecDeque::new(),
            exit_cfg: exit_cfg.clone(),
        }
    }

    /// The exit policy this variant replays (tests/observability).
    pub fn exit_config(&self) -> &ExitConfig {
        &self.exit_cfg
    }

    /// Drop virtual positions whose token is no longer part of the live round
    /// (its market expired; live would have force-exited it). Closed-trade
    /// history is preserved so metrics accumulate across round boundaries (D-3).
    pub fn retain_tokens(&mut self, valid: &std::collections::HashSet<String>) {
        self.open.retain(|t, _| valid.contains(t));
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
/// params, `is_baseline=true`) plus `count-1` DIRECTED single-knob variants.
/// Mutations are deterministic (no RNG) so behaviour is reproducible and testable.
///
/// Directed (not uniform) mutation (D-3): each variant moves EXACTLY ONE knob,
/// alternating the defensive/aggressive side by index. The old behaviour scaled
/// all four fields by the same factor, which pulled the entry decision in two
/// opposing directions at once and left variants decision-neutral — so they could
/// never diverge enough to justify an evolution. One knob per variant makes any
/// real edge attributable to a single parameter.
pub fn build_variants(
    base: &MutableParams,
    count: usize,
    max_gradient: Decimal,
    exit_cfg: &ExitConfig,
    now_ms: i64,
) -> Vec<Variant> {
    let mut out = Vec::with_capacity(count.max(1));
    out.push(Variant::new("baseline".into(), "live(baseline)".into(), base.clone(), true, now_ms, exit_cfg));

    // One knob per variant, cycling the four tunables. Odd indices take the
    // CONSERVATIVE direction (tighter cap / lower entry / higher bars), even
    // indices the AGGRESSIVE side, so the set explores both without exceeding the
    // gradient lock.
    let knobs: [&str; 4] = [
        "trend_max_entry_price",
        "trend_entry_factor",
        "trend_broken_price",
        "trend_min_price",
    ];
    // "Conservative" means a stricter entry: lower the price caps, raise the bars.
    let tighten = [Decimal::new(97, 2), Decimal::new(97, 2), Decimal::new(103, 2), Decimal::new(103, 2)];
    let loosen = [Decimal::new(103, 2), Decimal::new(103, 2), Decimal::new(97, 2), Decimal::new(97, 2)];
    for i in 1..count {
        let k = (i - 1) % knobs.len();
        let factor = if i % 2 == 1 { tighten[k] } else { loosen[k] };
        let name = knobs[k];
        let mut params = base.clone();
        params.set(name, base.get(name) * factor);
        // Never let a directed step breach the ±max_gradient lock.
        params = super::guard::clamped_step(base, &params, max_gradient);
        out.push(Variant::new(
            format!("variant-{i}"),
            format!("variant-{i}({name} x{factor})"),
            params,
            false,
            now_ms,
            exit_cfg,
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
        let exit = ExitConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &exit);
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
        let exit = ExitConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &exit);
        v.on_tick("t", &book(0.43, 0.45), false, 900_000, 1000);
        assert_eq!(v.open_positions(), 0);
    }

    #[test]
    fn metrics_prune_outside_the_window() {
        let exit = ExitConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &exit);
        v.on_tick("t", &book(0.43, 0.45), true, 900_000, 0);
        v.on_tick("t", &book(0.95, 0.97), true, 900_000, 1_000);
        assert_eq!(v.metrics(1800, 1_000).sample_count, 1);
        // Far in the future, the trade falls outside a 30-min window.
        assert_eq!(v.metrics(1800, 10_000_000).sample_count, 0);
    }

    #[test]
    fn variants_are_within_gradient_and_deterministic() {
        let base = MutableParams::default();
        let exit = ExitConfig::default();
        let vs = build_variants(&base, 5, dec!(0.05), &exit, 0);
        assert_eq!(vs.len(), 5);
        assert!(vs[0].is_baseline);
        for v in vs.iter().skip(1) {
            assert!(super::super::guard::validate_gradient(&base, &v.params, dec!(0.05)).is_ok());
        }
        let vs2 = build_variants(&base, 5, dec!(0.05), &exit, 0);
        assert_eq!(vs[1].params, vs2[1].params); // deterministic
    }

    #[test]
    fn directed_variants_move_exactly_one_knob() {
        // D-3: a variant must differ from the base in ONE field only, so any edge
        // is attributable to a single parameter.
        let base = MutableParams::default();
        let exit = ExitConfig::default();
        let vs = build_variants(&base, 4, dec!(0.05), &exit, 0);
        for v in vs.iter().skip(1) {
            let changed = base
                .fields()
                .iter()
                .zip(v.params.fields().iter())
                .filter(|((_, a), (_, b))| a != b)
                .count();
            assert_eq!(changed, 1, "variant {} changed {changed} knobs", v.id);
        }
    }

    #[test]
    fn variant_uses_the_live_exit_config_not_a_fabricated_stop() {
        // D-2: the simulated SL must equal the live ExitConfig's SL, never the
        // immutable 50% hard stop.
        let mut exit = ExitConfig::default();
        exit.stop_loss_pct = dec!(7);
        let v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &exit);
        assert_eq!(v.exit_config().stop_loss_pct, dec!(7));
    }

    #[test]
    fn closed_history_survives_a_round_reset() {
        // D-3: pruning a new round's vanished tokens keeps accumulated trades.
        let exit = ExitConfig::default();
        let mut v = Variant::new("v".into(), "v".into(), MutableParams::default(), false, 0, &exit);
        v.on_tick("old", &book(0.43, 0.45), true, 900_000, 1000);
        v.on_tick("old", &book(0.95, 0.97), true, 900_000, 2000);
        assert_eq!(v.metrics(1800, 2000).sample_count, 1);
        let valid: std::collections::HashSet<String> = ["new".to_string()].into_iter().collect();
        v.retain_tokens(&valid);
        assert_eq!(v.open_positions(), 0);
        assert_eq!(v.metrics(1800, 2000).sample_count, 1); // trade kept
    }
}
