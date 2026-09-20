//! MarketRegime — a bounded online state machine that tracks which regime the
//! target token's mid price is in (E16 / #98).
//!
//! Two consumers share ONE classification rule, by construction:
//!
//!  - the **offline labelling rule** (`classify_window`) turns a full window of
//!    mid samples into a ground-truth label — that is the labelled set the
//!    evaluation protocol is defined against;
//!  - the **online state machine** (`MarketRegime`) reproduces the same rule
//!    incrementally from a bounded ring of recent samples, with a confirmation
//!    hysteresis so a stray tick cannot flip the state.
//!
//! The acceptance metric (accuracy ≥ 80%) is then a real measurement: how well
//! the online machine tracks the offline rule on the same windows. It is NOT a
//! tautology — the ring is an approximation of the window and the confirmation
//! lag is the machine's real cost on transition windows.
//!
//! Everything is `Decimal` in/out, deterministic given the tick sequence, no
//! clocks or I/O — same rules as the rest of this crate.

use rust_decimal::Decimal;
use std::collections::VecDeque;

/// One tick of a binary market price (0.01). Thresholds are expressed in ticks
/// so the constants read the same on every market regardless of price level.
/// `from_parts` (mantissa 1, scale 2) because `Decimal::new` is not const.
pub const PRICE_TICK: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

/// The four regimes. `Range` is the default so a machine that has seen nothing
/// sensible yet answers conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Regime {
    /// Small net move, modest path jitter.
    #[default]
    Range,
    /// Net move up over the window with directional efficiency above the bar.
    TrendUp,
    /// Net move down, symmetric to `TrendUp`.
    TrendDown,
    /// Per-step movement so large that any direction signal is noise.
    Volatile,
}

impl Regime {
    pub fn as_str(&self) -> &'static str {
        match self {
            Regime::Range => "range",
            Regime::TrendUp => "trendUp",
            Regime::TrendDown => "trendDown",
            Regime::Volatile => "volatile",
        }
    }

    pub fn parse(s: &str) -> Option<Regime> {
        match s {
            "range" => Some(Regime::Range),
            "trendUp" => Some(Regime::TrendUp),
            "trendDown" => Some(Regime::TrendDown),
            "volatile" => Some(Regime::Volatile),
            _ => None,
        }
    }
}

/// Thresholds of the labelling rule. The same constants drive the offline
/// label and the online machine, so "accuracy" measures the online estimator,
/// never a second opinion about what a regime is.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketRegimeConfig {
    /// Evaluation window in ms. The offline label is computed over exactly
    /// this much wall clock; the online ring prunes samples older than this.
    pub window_ms: i64,
    /// |net move| must reach this many ticks (with the efficiency bar) to
    /// call a trend.
    pub trend_net_ticks: Decimal,
    /// Directional efficiency |net| / path must be ≥ this for a trend.
    pub trend_min_efficiency: Decimal,
    /// Mean absolute per-step move ≥ this many ticks (and not a trend) →
    /// `Volatile`.
    pub volatile_mad_ticks: Decimal,
    /// Consecutive agreeing raw classifications required before the state
    /// switches (hysteresis).
    pub confirmations: u32,
    /// Hard cap on ring samples (oldest dropped). A bursty feed must not grow
    /// the machine without bound.
    pub max_samples: usize,
}

impl Default for MarketRegimeConfig {
    fn default() -> Self {
        Self {
            window_ms: 300_000,
            trend_net_ticks: Decimal::new(3, 0),
            trend_min_efficiency: Decimal::new(5, 1),
            volatile_mad_ticks: Decimal::new(15, 1),
            confirmations: 2,
            max_samples: 4096,
        }
    }
}

/// Per-window statistics the label is computed from (also what the eval report
/// echoes so the thresholds stay auditable).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowStats {
    /// Last − first, in ticks (signed).
    pub net_ticks: Decimal,
    /// (last − first) / Σ|Δp|, in [−1, 1]; 0 when the path is flat.
    pub efficiency: Decimal,
    /// Σ|Δp| / steps, in ticks — the jitter measure.
    pub mad_ticks: Decimal,
    pub samples: usize,
}

/// The offline labelling rule over a full window of `(at_ms, mid)` samples.
/// This is the single definition of "what label does this window get".
pub fn classify_window(
    samples: &[(i64, Decimal)],
    config: &MarketRegimeConfig,
) -> (Regime, WindowStats) {
    let stats = window_stats(samples);
    (regime_from_stats(&stats, config), stats)
}

/// Same statistics, exposed for eval reports.
pub fn window_stats(samples: &[(i64, Decimal)]) -> WindowStats {
    let mut net = Decimal::ZERO;
    let mut path = Decimal::ZERO;
    if let Some(first) = samples.first() {
        net = first.1;
    }
    let mut prev = samples.first().map(|s| s.1);
    for (_, p) in samples.iter() {
        if let Some(prev_p) = prev {
            path += (p - prev_p).abs();
        }
        prev = Some(*p);
    }
    if let Some(last) = samples.last() {
        net = last.1 - net;
    }
    let steps = samples.len().saturating_sub(1);
    let efficiency = if path.is_zero() {
        Decimal::ZERO
    } else {
        net / path
    };
    let mad = if steps == 0 {
        Decimal::ZERO
    } else {
        path / Decimal::from(steps)
    };
    WindowStats {
        net_ticks: net / PRICE_TICK,
        efficiency,
        mad_ticks: mad / PRICE_TICK,
        samples: samples.len(),
    }
}

/// The rule itself: stats → label. Shared by the offline label and the online
/// machine's raw classification.
pub fn regime_from_stats(s: &WindowStats, config: &MarketRegimeConfig) -> Regime {
    if s.net_ticks >= config.trend_net_ticks && s.efficiency >= config.trend_min_efficiency {
        Regime::TrendUp
    } else if s.net_ticks <= -config.trend_net_ticks && s.efficiency <= -config.trend_min_efficiency
    {
        Regime::TrendDown
    } else if s.mad_ticks >= config.volatile_mad_ticks {
        Regime::Volatile
    } else {
        Regime::Range
    }
}

/// The online state machine. Feed it every mid price of one token; it keeps a
/// ring of the last `window_ms` worth of samples and switches its state only
/// after `confirmations` consecutive agreeing raw classifications.
#[derive(Debug)]
pub struct MarketRegime {
    config: MarketRegimeConfig,
    samples: VecDeque<(i64, Decimal)>,
    state: Regime,
    pending: Option<Regime>,
    pending_count: u32,
    updates: u64,
}

impl MarketRegime {
    pub fn new(config: MarketRegimeConfig) -> Self {
        Self {
            config,
            samples: VecDeque::new(),
            state: Regime::default(),
            pending: None,
            pending_count: 0,
            updates: 0,
        }
    }

    pub fn state(&self) -> Regime {
        self.state
    }

    pub fn config(&self) -> &MarketRegimeConfig {
        &self.config
    }

    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Feed one mid sample. Returns the (possibly still previous) state after
    /// the update — the value an eval harness samples at each window edge.
    pub fn on_price(&mut self, at_ms: i64, mid: Decimal) -> Regime {
        self.updates += 1;
        // Timestamps arrive in order from every source this crate is fed by;
        // a stray regression prunes nothing extra but is otherwise harmless.
        self.samples.push_back((at_ms, mid));
        let edge = at_ms - self.config.window_ms;
        while let Some(front) = self.samples.front() {
            if front.0 < edge {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        if self.samples.len() > self.config.max_samples {
            let overflow = self.samples.len() - self.config.max_samples;
            self.samples.drain(0..overflow);
        }
        let (_, stats) = classify_window(self.samples.make_contiguous(), &self.config);
        let raw = regime_from_stats(&stats, &self.config);
        if raw == self.state {
            self.pending = None;
            self.pending_count = 0;
        } else {
            if self.pending != Some(raw) {
                self.pending = Some(raw);
                self.pending_count = 0;
            }
            self.pending_count += 1;
            if self.pending_count >= self.config.confirmations.max(1) {
                self.state = raw;
                self.pending = None;
                self.pending_count = 0;
            }
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn cfg() -> MarketRegimeConfig {
        MarketRegimeConfig::default()
    }

    fn series(step_cents: i64, n: usize) -> Vec<(i64, Decimal)> {
        // Starts at 0.40, one sample per second.
        (0..n)
            .map(|i| {
                (
                    1_000_000 + i as i64 * 1_000,
                    dec!(0.40) + Decimal::from(step_cents * i as i64) * PRICE_TICK,
                )
            })
            .collect()
    }

    #[test]
    fn flat_series_is_range() {
        let flat: Vec<(i64, Decimal)> = (0..10)
            .map(|i| (1_000_000 + i * 1_000, dec!(0.40)))
            .collect();
        let (label, stats) = classify_window(&flat, &cfg());
        assert_eq!(label, Regime::Range);
        assert_eq!(stats.net_ticks, Decimal::ZERO);
        assert_eq!(stats.mad_ticks, Decimal::ZERO);
        assert_eq!(stats.samples, 10);
    }

    #[test]
    fn steady_rise_is_trend_up_and_fall_is_trend_down() {
        let (up, _) = classify_window(&series(1, 30), &cfg());
        assert_eq!(up, Regime::TrendUp, "net 29 ticks, efficiency 1.0");
        let down: Vec<(i64, Decimal)> = series(1, 30)
            .into_iter()
            .map(|(t, p)| (t, dec!(0.69) - (p - dec!(0.40))))
            .collect();
        let (dn, _) = classify_window(&down, &cfg());
        assert_eq!(dn, Regime::TrendDown);
    }

    #[test]
    fn small_chop_is_range_but_big_chop_is_volatile() {
        // ±1 tick oscillation: net 0, mad 1 tick < 1.5 → Range.
        let small: Vec<(i64, Decimal)> = (0..10)
            .map(|i| {
                let p = if i % 2 == 0 { dec!(0.40) } else { dec!(0.41) };
                (1_000_000 + i * 1_000, p)
            })
            .collect();
        let (label, _) = classify_window(&small, &cfg());
        assert_eq!(label, Regime::Range);
        // ±2 tick oscillation: mad 2 ticks ≥ 1.5 → Volatile (not a trend: net 0).
        let big: Vec<(i64, Decimal)> = (0..10)
            .map(|i| {
                let p = if i % 2 == 0 { dec!(0.40) } else { dec!(0.42) };
                (1_000_000 + i * 1_000, p)
            })
            .collect();
        let (label, _) = classify_window(&big, &cfg());
        assert_eq!(label, Regime::Volatile);
    }

    #[test]
    fn machine_confirms_before_switching() {
        let mut m = MarketRegime::new(cfg());
        let now = 1_000_000i64;
        // A clean rise: raw is TrendUp from the second sample, but the state
        // must hold Range until `confirmations` agreeing calls.
        let mut last = Regime::default();
        for i in 0..30usize {
            last = m.on_price(
                now + i as i64 * 1_000,
                dec!(0.40) + Decimal::from(i as u32) * PRICE_TICK,
            );
            if (i as u32) < m.config.confirmations - 1 {
                assert_eq!(last, Regime::Range, "hysteresis holds through call {i}");
            }
        }
        assert_eq!(last, Regime::TrendUp);
    }

    #[test]
    fn machine_prunes_the_ring_so_old_moves_expire() {
        let mut m = MarketRegime::new(MarketRegimeConfig {
            window_ms: 10_000,
            ..cfg()
        });
        let now = 1_000_000i64;
        // Strong rise inside the first window → TrendUp.
        for i in 0..20 {
            m.on_price(now + i * 500, dec!(0.40) + Decimal::from(i) * PRICE_TICK);
        }
        assert_eq!(m.state(), Regime::TrendUp);
        // Then a long flat stretch entirely beyond the window: the ring forgets
        // the rise and the machine settles back to Range.
        let settle_at = now + 30_000;
        for i in 0..30 {
            m.on_price(settle_at + i * 500, dec!(0.59));
        }
        assert_eq!(m.state(), Regime::Range);
    }

    #[test]
    fn machine_ring_respects_max_samples() {
        let mut m = MarketRegime::new(MarketRegimeConfig {
            max_samples: 8,
            ..cfg()
        });
        let now = 1_000_000i64;
        for i in 0..50 {
            m.on_price(now + i * 1_000, dec!(0.40) + Decimal::from(i) * PRICE_TICK);
        }
        assert_eq!(m.samples.len(), 8);
    }
}
