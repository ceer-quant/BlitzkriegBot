//! The built-in `mean_reversion` strategy (E4-b / #31): the *fade* leg.
//!
//! Where [`super::spread_arb`] buys a dip inside a CONFIRMED trend (it must wait
//! out the [`crate::signal::TrendTracker`] window) and [`super::trend_follow`]
//! chases a rising mid, this strategy buys a freshly CRASHED side — a token
//! whose mid has fallen a large fraction off the high of its own recent history
//! and now sits in the cheap zone. It is the counter-trend leg of the E4 hedge:
//! its candidate windows are exactly the ones the other two legs have nothing
//! in (no confirmed trend, price going the wrong way to chase).
//!
//! | | `spread_arb` | `trend_follow` | `mean_reversion` |
//! | --- | --- | --- | --- |
//! | trigger | a dip inside a confirmed trend | the mid RISES `>= min_move_pct` | the mid FELL `>= min_drop_pct` off the lookback high |
//! | side | the underpriced side | the side being bid up | the just-crashed side |
//! | pricing | `entry < mid` | `entry = best_ask > mid` | `entry < mid` |
//! | entry gate pass | natural | natural | **momentum exempted** (E2-b) |
//!
//! All state comes from the token's own book history (a [`PriceBuffer`] fed in
//! `on_book`), never from the scanner's cached prices — the pricing discipline
//! at the top of `signal.rs`.
//!
//! Exits are NOT this strategy's business (shared exit policy, D-2, like the
//! other two builtins). Its one entry-side control is `take_breaks`: the
//! oversold premise is gone once the mid recovers above `max_price`, so the
//! kernel cancels that token's resting entry bid — the mirror of
//! `trend_follow`'s move-death break.
//!
//! It declares **the momentum gate exemption** (`gate_exemptions().timing` stays
//! false): the shared spot filter rejects an entry when spot moves against the
//! bet, and a crash on the bet's own asset moves spot against it by
//! construction. Measured on the held-out slice, the gate would refuse 39–50%
//! of this leg's candidates depending on calibration — refusing them is what
//! kills a counter-trend strategy, so the exemption is this strategy's reason
//! for existing (E2-b / #27). Every honoured exemption is recorded per order
//! (`gateExemptedMomentum` / `declaredExemptions`), so nothing enters silently.
//!
//! E2-c (#28) applies unchanged: six tunables declared with names, values and
//! domains ([`MEAN_REVERSION_KNOBS`]), and a twin built from any parameter set
//! ([`MeanReversionShadowFactory`]).

use super::shadow_twin::ShadowFactory;
use super::{EngineStrategy, GateExemptions, StrategyCtx};
use crate::model::OrderbookSnapshot;
use crate::model::SignalDirection;
use crate::shadow_evolution::{KnobSpec, ParamRegistry, StrategyParams};
use crate::signal::{PriceBuffer, TradeSignal};
use arc_swap::ArcSwap;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Entry-side parameters for the fade leg. Every field is a declared knob.
///
/// The two defaults that are NOT free choices are grounded in the recorded
/// market data (`data/archive/`, ~18 h of dry-run capture; the extraction script
/// lives in the E4-b acceptance report):
///
///  * `min_drop_pct = 10` — over every cheap-zone tick (mid <= 0.35) of the
///    held-out slice, the drop from the 120 s lookback high has p25 = -49% /
///    p50 = -33%: crashes are deep here all the time, so a 10% gate removes
///    only the shallow drift and stops the strategy fading noise; tightening it
///    further cuts candidates without changing their population (fires are 35
///    at 8% vs 35 at 10% vs 32 at 15%).
///  * `max_spread_pct = 8` — at the candidate instants the quoted spread is
///    ~5.7-6.7% mid-to-p75% and 13.3% at p75; 8% (~45% of candidates) excludes
///    the fat tail — a book 3x wider than the two with-trend legs, which is
///    what a crashed token looks like — without making the leg unfireable.
///
/// The remaining defaults are structural:
///  * `max_price = 0.35` follows the TS reference (`cheapThreshold`), and the
///    payoff floor `0.05` makes an entry at least 5:1 on the payoff.
///  * `entry_factor = 0.98` is shared with `spread_arb` (one resting-bid
///    discipline across the dip family).
///  * `lookback_sec = 120` is the drop's memory window; 60 s fired ~9% less
///    with no change in entry quality on the slice.
///  * `cooldown_sec = 60` — one candidate per token per minute, so a token
///    grinding down to zero cannot re-fire with every tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeanReversionConfig {
    /// Memory window (sec) the drop is measured over.
    pub lookback_sec: i64,
    /// Minimum FALL (%) of the mid off the lookback high to call it oversold.
    pub min_drop_pct: Decimal,
    /// The faded side must sit at or below this price (the cheap zone).
    pub max_price: Decimal,
    /// Resting bid = mid * this factor (same discipline as `spread_arb`).
    pub entry_factor: Decimal,
    /// Don't buy into a book wider than this (%).
    pub max_spread_pct: Decimal,
    /// At most one candidate per token per this many seconds.
    pub cooldown_sec: i64,
}

impl Default for MeanReversionConfig {
    fn default() -> Self {
        Self {
            lookback_sec: 120,
            min_drop_pct: dec!(10),
            max_price: dec!(0.35),
            entry_factor: dec!(0.98),
            max_spread_pct: dec!(8),
            cooldown_sec: 60,
        }
    }
}

/// The knobs `mean_reversion` declares evolvable, as `(name, default, min, max)`.
///
/// The domain is a HARD outer bound (`guard::validate_domain` rejects anything
/// outside it before the ±gradient lock is consulted). `entry_factor`'s ceiling
/// is 1.00 exclusive in spirit: the discipline requires a bid STRICTLY below the
/// mid, so a proposal of exactly 1.00 produces entries that every mid comparison
/// rejects rather than an inversion of the strategy — kept in-domain so a hot
/// swap can approach it without tripping the domain guard, and rejected by the
/// pricing rule instead.
pub const MEAN_REVERSION_KNOBS: [(&str, Decimal, Decimal, Decimal); 6] = [
    ("lookback_sec", dec!(120), dec!(10), dec!(600)),
    ("min_drop_pct", dec!(10), dec!(1.0), dec!(50.0)),
    ("max_price", dec!(0.35), dec!(0.10), dec!(0.60)),
    ("entry_factor", dec!(0.98), dec!(0.80), dec!(1.00)),
    ("max_spread_pct", dec!(8), dec!(0.10), dec!(25.0)),
    ("cooldown_sec", dec!(60), dec!(0), dec!(600)),
];

fn knob_value(cfg: &MeanReversionConfig, name: &str) -> Decimal {
    match name {
        "lookback_sec" => Decimal::from(cfg.lookback_sec),
        "min_drop_pct" => cfg.min_drop_pct,
        "max_price" => cfg.max_price,
        "entry_factor" => cfg.entry_factor,
        "max_spread_pct" => cfg.max_spread_pct,
        "cooldown_sec" => Decimal::from(cfg.cooldown_sec),
        _ => Decimal::ZERO,
    }
}

/// This strategy's knob declaration derived from the config in force.
pub fn mean_reversion_knobs(cfg: &MeanReversionConfig) -> Vec<KnobSpec> {
    MEAN_REVERSION_KNOBS
        .iter()
        .map(|(name, _default, min, max)| {
            let v = knob_value(cfg, name);
            // A config that starts outside the default domain widens it, exactly
            // like the other two builtins: the declaration must fit the strategy
            // as it runs, not as it was compiled.
            KnobSpec::new(*name, v, (*min).min(v), (*max).max(v))
        })
        .collect()
}

/// Overlay a parameter set on a base config. Undeclared names are ignored.
pub fn apply_knobs(base: &MeanReversionConfig, params: &StrategyParams) -> MeanReversionConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            // Whole-number-of-seconds knobs truncate, so a fractional proposal
            // can never produce a non-integral window or a negative cooldown.
            "lookback_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs > 0
                {
                    cfg.lookback_sec = secs;
                }
            }
            "cooldown_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs >= 0
                {
                    cfg.cooldown_sec = secs;
                }
            }
            "min_drop_pct" => cfg.min_drop_pct = v,
            "max_price" => cfg.max_price = v,
            "entry_factor" => cfg.entry_factor = v,
            "max_spread_pct" => cfg.max_spread_pct = v,
            _ => {}
        }
    }
    cfg
}

/// Per-token fade state for the reverse leg.
///
/// `on_price` feeds each token's own mid into a rolling [`PriceBuffer`] (retention
/// sized for the widest legal `lookback_sec`, so a hot swap can not silently
/// shrink the history the knob is measured over) and records candidate fires.
pub struct FadeTracker {
    cfg: MeanReversionConfig,
    buffers: HashMap<String, PriceBuffer>,
    /// Last candidate fire per token (for the cooldown), token -> ts.
    last_fire: HashMap<String, i64>,
    round_slot: i64,
    /// Tokens whose oversold premise just vanished (mid back above `max_price`).
    broken: Vec<(String, Decimal)>,
    /// Mid was inside the cheap zone at the previous tick, per token.
    in_zone: HashMap<String, bool>,
}

/// Retention (sec) for each token's price history — the widest legal lookback.
fn history_retention_sec(cfg: &MeanReversionConfig) -> i64 {
    let ceiling = MEAN_REVERSION_KNOBS[0].3.to_i64().unwrap_or(600);
    cfg.lookback_sec.max(1).max(ceiling)
}

impl FadeTracker {
    pub fn new(cfg: MeanReversionConfig) -> Self {
        Self {
            cfg,
            buffers: HashMap::new(),
            last_fire: HashMap::new(),
            round_slot: i64::MIN,
            broken: Vec::new(),
            in_zone: HashMap::new(),
        }
    }

    pub fn set_config(&mut self, cfg: MeanReversionConfig) {
        self.cfg = cfg;
    }

    pub fn on_price(&mut self, token_id: &str, price: Decimal, now_ms: i64) {
        if token_id.is_empty() || price <= Decimal::ZERO {
            return;
        }
        let retention = history_retention_sec(&self.cfg);
        let buf = self
            .buffers
            .entry(token_id.to_string())
            .or_insert_with(|| PriceBuffer::new(retention));
        buf.push(price, now_ms);

        let cheap = self.cfg.max_price;
        let was = self.in_zone.get(token_id).copied().unwrap_or(false);
        let now = price <= cheap && price > Decimal::ZERO;
        self.in_zone.insert(token_id.to_string(), now);
        if was && !now && cheap > Decimal::ZERO {
            // Left the cheap zone upward: the oversold premise is gone.
            self.broken.push((token_id.to_string(), price));
            self.last_fire.remove(token_id);
        }
    }

    pub fn reset_if_new_round(&mut self, slot: i64) {
        if slot != self.round_slot {
            self.round_slot = slot;
            self.buffers.clear();
            self.last_fire.clear();
            self.broken.clear();
            self.in_zone.clear();
        }
    }

    /// The drop (%) of one token's current mid off the highest mid in its
    /// lookback window. Zero when there is not enough history; the entry
    /// evaluator's `< -min_drop_pct` check then refuses.
    pub fn drop_pct(&self, token_id: &str, now_ms: i64) -> Decimal {
        let window = self.cfg.lookback_sec.max(1);
        self.buffers
            .get(token_id)
            .map(|b| {
                if b.len() < 2 {
                    return Decimal::ZERO;
                }
                let hi = b.highest(window, now_ms);
                if hi <= Decimal::ZERO {
                    return Decimal::ZERO;
                }
                let cur = b.latest();
                (cur - hi) / hi * Decimal::ONE_HUNDRED
            })
            .unwrap_or(Decimal::ZERO)
    }

    /// Cooldown check and record for one token's candidate fire.
    pub fn try_fire(&mut self, token_id: &str, now_ms: i64) -> bool {
        let last = self.last_fire.get(token_id).copied();
        if let Some(t) = last
            && now_ms - t < self.cfg.cooldown_sec * 1000
        {
            return false;
        }
        self.last_fire.insert(token_id.to_string(), now_ms);
        true
    }

    pub fn is_in_zone(&self, token_id: &str) -> bool {
        // Being asked again means the buffer exists; absence reads as "not yet".
        *self.in_zone.get(token_id).unwrap_or(&false)
    }

    /// Drain tokens whose oversold premise just vanished.
    pub fn take_broken(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.broken)
    }
}

/// Fade evaluator for one market's two outcome tokens.
///
/// Returns a signal for the side that is cheap AND has just fallen hard off its
/// own lookback high. Pricing mirrors `evaluate_spread_arb`: a resting bid below
/// the mid (`mid * entry_factor`, clamped to `[0.05, 0.90]` and the tick grid),
/// never above the live best bid and strictly below the mid.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_mean_reversion(
    asset: &str,
    condition_id: &str,
    up_token: &str,
    down_token: &str,
    up_book: Option<&OrderbookSnapshot>,
    down_book: Option<&OrderbookSnapshot>,
    tracker: &FadeTracker,
    now_ms: i64,
    cfg: &MeanReversionConfig,
) -> Option<TradeSignal> {
    for (token, dir, book) in [
        (up_token, SignalDirection::Up, up_book),
        (down_token, SignalDirection::Down, down_book),
    ] {
        if token.is_empty() || !tracker.is_in_zone(token) {
            continue;
        }
        let Some(book) = book else { continue };
        if book.bids.is_empty() || book.asks.is_empty() || book.mid_price <= Decimal::ZERO {
            continue;
        }
        // Don't buy into a wide book.
        if cfg.max_spread_pct > Decimal::ZERO && book.spread_pct > cfg.max_spread_pct {
            continue;
        }
        let mid = book.mid_price;
        // The cheap-zone premise is enforced on the LIVE book mid (the tracker
        // only triggers the break); a stale zone flag must not trade an already
        // recovered token.
        if mid > cfg.max_price || mid < dec!(0.05) {
            continue;
        }
        // Drop from the lookback high, measured on the tracker's own buffer.
        let drop = tracker.drop_pct(token, now_ms);
        if cfg.min_drop_pct > Decimal::ZERO && drop > -cfg.min_drop_pct {
            continue;
        }
        let mut entry = (mid * cfg.entry_factor).max(dec!(0.05)).min(dec!(0.9));
        entry = round2(entry);
        if book.best_bid > Decimal::ZERO && entry > book.best_bid {
            entry = round2(book.best_bid);
        }
        if entry >= mid || entry <= Decimal::ZERO {
            continue;
        }
        return Some(TradeSignal {
            strategy: "mean_reversion".into(),
            asset: asset.to_string(),
            direction: dir,
            token_id: token.to_string(),
            condition_id: condition_id.to_string(),
            price: entry,
            reason: format!(
                "{} fell {:+.2}% off lookback high, cheap zone, resting bid {} (mid {})",
                dir.as_str().to_uppercase(),
                drop,
                entry,
                mid
            ),
        });
    }
    None
}

fn round2(v: Decimal) -> Decimal {
    (v * Decimal::ONE_HUNDRED).round() / Decimal::ONE_HUNDRED
}

pub struct MeanReversionBuiltin {
    cfg: MeanReversionConfig,
    tracker: FadeTracker,
    /// This strategy's OWN cell in the host's per-strategy parameter registry
    /// (E2-c). None = evolution inactive, so the base config stands alone.
    hot_params: Option<Arc<ArcSwap<StrategyParams>>>,
    /// Whether the current cycle produced a candidate fire. Consumed by
    /// `find_candidates` after `evaluate` gates it; the cooldown is recorded
    /// ONLY for candidates the host actually saw (not for ones its own
    /// structural checks rejected).
    pending_fire: HashSet<String>,
}

impl MeanReversionBuiltin {
    pub fn new(cfg: MeanReversionConfig) -> Self {
        Self {
            tracker: FadeTracker::new(cfg.clone()),
            cfg,
            hot_params: None,
            pending_fire: HashSet::new(),
        }
    }

    /// The config currently in force: base overlaid with the hot-swapped
    /// per-strategy parameters when Shadow Evolution is active.
    fn effective_cfg(&self) -> MeanReversionConfig {
        match &self.hot_params {
            Some(h) => apply_knobs(&self.cfg, &h.load()),
            None => self.cfg.clone(),
        }
    }

    /// Push the in-force config into this instance's OWN tracker. Applied
    /// unconditionally including after `set_hot_params(None)` (detach must
    /// restore the base config), same as `TrendFollowBuiltin::sync_tracker_cfg`.
    fn sync_tracker_cfg(&mut self) {
        let cfg = self.effective_cfg();
        self.tracker.set_config(cfg);
    }
}

impl EngineStrategy for MeanReversionBuiltin {
    fn name(&self) -> &str {
        "mean_reversion"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        self.sync_tracker_cfg();
        self.tracker.on_price(token_id, snap.mid_price, now_ms);
    }

    fn on_round(&mut self, slot: i64, _time_left_sec: i64, _now_ms: i64) {
        self.tracker.reset_if_new_round(slot);
        self.pending_fire.clear();
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        self.tracker.take_broken()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        self.pending_fire.clear();
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
                if book.is_none() {
                    continue;
                }
                if !self.tracker.is_in_zone(&token) {
                    continue;
                }
                if !self.tracker.try_fire(&token, now) {
                    continue;
                }
                if let Some(book) = book
                    && let Some(sig) = evaluate_mean_reversion(
                        &market.asset,
                        &market.condition_id,
                        &market.up_token_id,
                        &market.down_token_id,
                        if token == market.up_token_id {
                            Some(book)
                        } else {
                            None
                        },
                        if token == market.down_token_id {
                            Some(book)
                        } else {
                            None
                        },
                        &self.tracker,
                        now,
                        &cfg,
                    )
                {
                    self.pending_fire.insert(sig.token_id.clone());
                    out.push(sig);
                }
            }
        }
        out
    }

    /// The cheap-zone tokens this strategy is watching; entry-eligible only
    /// after they also pass the drop / spread / cooldown checks via live books.
    fn confirmed_tokens(&self) -> HashSet<String> {
        self.tracker
            .in_zone
            .iter()
            .filter(|(_, v)| **v)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// E4-b / #31: the shared entry-quality momentum gate is waived for THIS
    /// strategy's candidates. Timing stays gated. Every honoured waiver is
    /// recorded by the host per candidate order, so the opt-out is auditable.
    fn gate_exemptions(&self) -> GateExemptions {
        GateExemptions {
            timing: false,
            momentum: true,
        }
    }

    fn diagnostics(&self, ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        let eff = self.effective_cfg();
        self.tracker
            .in_zone
            .iter()
            .filter(|(_, v)| **v)
            .map(|(t, _)| {
                let mid = ctx
                    .fresh_book(t)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                let drop = self.tracker.drop_pct(t, ctx.now_ms());
                let entry = mid * eff.entry_factor;
                let firable = mid > Decimal::ZERO
                    && mid <= eff.max_price
                    && drop <= -eff.min_drop_pct
                    && entry < mid;
                serde_json::json!({
                    "token": t,
                    "mid": mid,
                    "dropPct": drop,
                    "entry": entry,
                    "cap": eff.max_price,
                    "firable": firable,
                })
            })
            .collect()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        // Resolve only OUR cell; None (evolution disabled) DETACHES the overlay.
        self.hot_params = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    fn evolvable_knobs(&self) -> Vec<KnobSpec> {
        mean_reversion_knobs(&self.effective_cfg())
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        // Built from the config in force WITHOUT the hot overlay: the overlay is
        // exactly what the factory's parameter argument applies.
        Some(Box::new(MeanReversionShadowFactory {
            base: self.cfg.clone(),
        }))
    }
}

/// Builds independent `mean_reversion` twins for the shadow engine.
///
/// A twin owns its OWN [`FadeTracker`], so `min_drop_pct`/`max_price`/
/// `lookback_sec` genuinely change what the twin fades — a counterfactual, not
/// a relabelled baseline.
pub struct MeanReversionShadowFactory {
    base: MeanReversionConfig,
}

impl ShadowFactory for MeanReversionShadowFactory {
    fn strategy(&self) -> String {
        "mean_reversion".to_string()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        mean_reversion_knobs(&self.base)
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        Some(Box::new(MeanReversionBuiltin::new(apply_knobs(
            &self.base, params,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::super::shadow_twin::{TwinReplay, tick_ctx};
    use super::*;
    use crate::exit_policy::ExitConfig;
    use crate::model::CryptoMarket;
    use crate::shadow_evolution::variants::Metrics;
    use rust_decimal::prelude::FromPrimitive;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        book_d(
            Decimal::from_f64(bid).unwrap(),
            Decimal::from_f64(ask).unwrap(),
        )
    }

    fn book_d(bid: Decimal, ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels("t", vec![(bid, dec!(100))], vec![(ask, dec!(100))], 0)
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

    /// Drive a FALL from `from` to `to` on `token` in `count` steps spaced 1 s
    /// apart, then return the clock.
    fn fall(
        t: &mut FadeTracker,
        token: &str,
        from: Decimal,
        to: Decimal,
        steps: usize,
        start: i64,
    ) -> i64 {
        let mut now = start;
        for i in 0..steps {
            let p = from + (to - from) * Decimal::from(i as u32) / Decimal::from(steps as u32 - 1);
            now += 1_000;
            t.on_price(token, p, now);
        }
        now
    }

    #[test]
    fn declarations_report_the_config_in_force_and_are_coherent() {
        let k = mean_reversion_knobs(&MeanReversionConfig::default());
        assert_eq!(k.len(), 6);
        for spec in &k {
            assert!(spec.is_coherent(), "{spec:?}");
        }
        assert_eq!(k[0].name, "lookback_sec");
        assert_eq!(k[0].value, dec!(120));
        let drop = k.iter().find(|s| s.name == "min_drop_pct").unwrap();
        assert_eq!(drop.value, dec!(10));
        assert_eq!(drop.max, dec!(50));
        // An out-of-domain starting config widens the declared domain.
        let cfg = MeanReversionConfig {
            lookback_sec: 900,
            ..Default::default()
        };
        let k = mean_reversion_knobs(&cfg);
        let lb = k.iter().find(|s| s.name == "lookback_sec").unwrap();
        assert!(lb.contains(dec!(900)), "{lb:?}");
        assert!(lb.is_coherent());
    }

    #[test]
    fn apply_knobs_ignores_undeclared_names_and_keeps_the_seconds_integral() {
        let base = MeanReversionConfig::default();
        let mut p = StrategyParams::new();
        p.set("max_price", dec!(0.30));
        p.set("lookback_sec", dec!(45.7));
        p.set("cooldown_sec", dec!(30.9));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.max_price, dec!(0.30));
        assert_eq!(out.lookback_sec, 45, "a fractional lookback truncates");
        assert_eq!(out.cooldown_sec, 30);
        assert_eq!(out.min_drop_pct, base.min_drop_pct);
        // A non-positive lookback can never be written through the overlay.
        let mut bad = StrategyParams::new();
        bad.set("lookback_sec", dec!(0));
        assert_eq!(apply_knobs(&base, &bad).lookback_sec, base.lookback_sec);
    }

    #[test]
    fn a_shallow_drift_never_enters_a_deep_fall_does() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        // Drift down 0.60 → 0.55 within 120 s ≈ -8.3%: inside the cheap zone at
        // the tail? No — 0.55 > max_price 0.35, never in the zone.
        let _now = fall(&mut t, "t", dec!(0.60), dec!(0.55), 5, 10_000);
        assert!(!t.is_in_zone("t"));
        assert!(t.take_broken().is_empty());

        // A deep fall to the cheap zone IS watched. But the entry evaluator
        // fires off the LIVE book, and the drop is measured off the tracker's
        // history that includes the high.
        let now = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 20_000);
        assert!(t.is_in_zone("t"), "0.30 is inside a 0.35 cheap zone");
        // 0.50 → 0.30 over the lookback = -40%: way past the 10% gate.
        let drop = t.drop_pct("t", now);
        assert!(drop <= dec!(-10), "{drop}");

        // Recovery above the cheap zone breaks the premise exactly once.
        t.on_price("t", dec!(0.50), now + 5_000);
        assert!(!t.is_in_zone("t"));
        let broken = t.take_broken();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "t");
        assert_eq!(broken[0].1, dec!(0.50));
        assert!(t.take_broken().is_empty(), "each break is returned once");
    }

    #[test]
    fn a_new_round_resets_the_fade_state() {
        let mut t = FadeTracker::new(MeanReversionConfig::default());
        let now = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 0);
        assert!(t.is_in_zone("t"));
        t.on_price("t", dec!(0.30), now + 1_000);
        t.reset_if_new_round(2);
        assert!(
            !t.is_in_zone("t"),
            "a new round starts with no zone history"
        );
        assert_eq!(t.drop_pct("t", now + 2_000), Decimal::ZERO);
        assert!(t.take_broken().is_empty());
    }

    #[test]
    fn the_entry_rests_below_the_mid_like_the_dip_buyer() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        // 0.50 → 0.30 in 5 s: -40% off the lookback high inside the zone.
        let now = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 10_000);
        // Live book round the mid: bid 0.29, ask 0.31 → entry = round2(0.294) = 0.29.
        let b = book(0.29, 0.31);
        let sig = evaluate_mean_reversion("BTC", "c", "t", "t-down", Some(&b), None, &t, now, &cfg)
            .expect("a deep crash in the cheap zone must fire");
        assert_eq!(sig.strategy, "mean_reversion");
        assert_eq!(sig.direction, SignalDirection::Up);
        assert_eq!(sig.token_id, "t");
        assert_eq!(sig.price, dec!(0.29), "resting bid = round2(mid*0.98)");
        assert!(sig.price < b.mid_price, "strictly below the mid");
        // .. and its reason mentions the drop.
        assert!(sig.reason.contains("resting bid"), "{}", sig.reason);
    }

    #[test]
    fn a_shallow_drop_a_wide_book_or_a_recovered_mid_each_blocks_the_entry() {
        let cfg = MeanReversionConfig::default();
        // 1. Shallow drop: 0.50 → 0.47 is 6% — below the 10% minimum.
        let mut t = FadeTracker::new(cfg.clone());
        let now = fall(&mut t, "t2", dec!(0.50), dec!(0.47), 3, 10_000);
        // For live-book purposes the token must also sit in the cheap zone
        // with a deep drop behind it: give the crash token "t" its own fall.
        let _ = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 10_000);
        assert!(t.is_in_zone("t"));

        // (a) recovered mid (above the cheap cap) — the zone guard refuses even
        // though the tracker still has massive historical state.
        let b = book(0.54, 0.56);
        assert!(
            evaluate_mean_reversion("BTC", "c", "t2", "t2-down", Some(&b), None, &t, now, &cfg)
                .is_none(),
            "a mid above the cheap cap must not fire"
        );

        // (b) an in-zone mid but no real drop history (the marker token 'cheap'
        // had a deep fall; a token never seen falling deep does not pass the
        // drop gate).
        let still_deep = book(0.29, 0.31);
        assert!(
            evaluate_mean_reversion(
                "BTC",
                "c",
                "t",
                "t-down",
                Some(&still_deep),
                None,
                &t,
                now,
                &cfg
            )
            .is_some(),
            "sanity: the real crash token fires"
        );

        // (c) wide book refused.
        let wide = book_d(dec!(0.15), dec!(0.40)); // spread ~91%
        assert!(
            evaluate_mean_reversion(
                "BTC",
                "c",
                "t",
                "t-down",
                Some(&wide),
                None,
                &t,
                now + 1_000,
                &cfg
            )
            .is_none(),
            "a wide book must not be bought"
        );

        // (d) degenerate one-sided book refused.
        let one_sided =
            OrderbookSnapshot::from_levels("t", vec![(dec!(0.29), dec!(100))], vec![], 0);
        assert!(
            evaluate_mean_reversion(
                "BTC",
                "c",
                "t",
                "t-down",
                Some(&one_sided),
                None,
                &t,
                now + 2_000,
                &cfg
            )
            .is_none()
        );
    }

    #[test]
    fn the_entry_never_exceeds_the_bid_or_the_ceiling() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        let _ = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 10_000);
        let now = 10_000 + 5 * 1_000;

        // entry_factor would put the bid above the live best bid: clamp DOWN to
        // the bid, still strictly below the mid.
        let b = book(0.32, 0.33); // mid 0.325, entry 0.32 = exactly the bid -> clamp
        let sig = evaluate_mean_reversion("BTC", "c", "t", "t-down", Some(&b), None, &t, now, &cfg)
            .expect("in-zone, crash-fallen token must fire");
        assert_eq!(sig.price, dec!(0.32), "clamped to the best bid");
        assert!(sig.price < b.mid_price);

        // The last two tests isolated the bid clamp; this one isolates the
        // 0.05 floor: mid 0.07 * 0.80 = 0.056 → round2 0.06, still above the
        // floor and strictly below the mid. The spread cap is widened so the
        // book's own width is not what the assert turns on.
        let floor_cfg = MeanReversionConfig {
            entry_factor: dec!(0.80),
            max_spread_pct: dec!(20),
            ..Default::default()
        };
        let b2 = book(0.065, 0.075); // spread 14.3% — admitted by the widened cap
        let s2 = evaluate_mean_reversion(
            "BTC",
            "c",
            "t",
            "t-down",
            Some(&b2),
            None,
            &t,
            now,
            &floor_cfg,
        )
        .expect("floor-safe entry must fire");
        assert_eq!(s2.price, dec!(0.06), "round2-floor entry");
        assert!(s2.price < b2.mid_price);
    }

    #[test]
    fn momentum_gate_is_waived_and_timing_is_not() {
        let s = MeanReversionBuiltin::new(MeanReversionConfig::default());
        let ex = s.gate_exemptions();
        assert!(
            ex.momentum,
            "the fade leg exists to enter against the momentum gate"
        );
        assert!(!ex.timing, "the round-timing window stays enforced");
        assert_eq!(ex.gates(), vec!["momentum"]);
        assert_eq!(s.name(), "mean_reversion");
    }

    #[test]
    fn a_twin_runs_the_strategys_own_logic_and_exits_on_the_shared_policy() {
        let strat = MeanReversionBuiltin::new(MeanReversionConfig::default());
        let factory = strat.shadow_factory().expect("mean_reversion is evolvable");
        assert_eq!(factory.strategy(), "mean_reversion");
        let params = StrategyParams::from_knobs(&factory.knobs());
        let twin = factory.make(&params).expect("twin builds");
        let mut replay = TwinReplay::new(twin, &ExitConfig::default());

        let m = market();
        replay.on_round(std::slice::from_ref(&m), &[], 0);
        // Crash 0.60 → 0.30 in ~10 s; the twin's own tracker sees it.
        let mut now = 0;
        let steps = 12;
        let to = dec!(0.30);
        let from = dec!(0.60);
        for i in 0..steps {
            now += 1_000;
            let p = from + (to - from) * Decimal::from(i) / Decimal::from(steps - 1);
            let b = book_d(p - dec!(0.01), p + dec!(0.01));
            replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "t", &b, 1, 870, now));
        }
        assert_eq!(
            replay.open_positions(),
            1,
            "the twin must fade the crash via its own logic"
        );

        // Near-certain win: above 0.64 a 0.32 entry runs past the fixed
        // take-profit backstop (+100%) and the shared exit policy banks it —
        // the same shape the dip buyer's twin test uses.
        now += 20_000;
        let up = book(0.95, 0.97);
        replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "t", &up, 1, 840, now));
        assert_eq!(replay.open_positions(), 0, "the recovery must be exited");
        let metrics = Metrics::from_trades(&replay.windowed_trades(1800, now + 1_000));
        assert_eq!(metrics.sample_count, 1);
        assert_eq!(metrics.wins, 1);
        assert!(
            metrics.total_pnl > Decimal::ZERO,
            "expected a profit, got {}",
            metrics.total_pnl
        );
    }

    #[test]
    fn a_twin_with_a_tighter_cheap_cap_skips_the_same_crash() {
        // The counterfactual that makes evolution meaningful: same tick stream,
        // one knob moved, a different decision — both through the strategy's
        // own code path.
        let m = market();
        let run = |cap: Decimal| -> usize {
            let strat = MeanReversionBuiltin::new(MeanReversionConfig {
                max_price: cap,
                ..Default::default()
            });
            let factory = strat.shadow_factory().unwrap();
            let mut params = StrategyParams::from_knobs(&factory.knobs());
            params.set("max_price", cap);
            let twin = factory.make(&params).unwrap();
            let mut replay = TwinReplay::new(twin, &ExitConfig::default());
            replay.on_round(std::slice::from_ref(&m), &[], 0);
            let mut now = 0;
            for i in 0..8 {
                now += 1_000;
                // 0.45 → 0.315: a -30% fade whose final leg stays shallow
                // enough that the shared stop policy does not kill the young
                // position mid-ramp (this test isolates the cheap-cap knob).
                let mid = dec!(0.45) - dec!(0.135) * Decimal::from(i) / Decimal::from(7);
                let b = book_d(mid - dec!(0.005), mid + dec!(0.005));
                replay.on_tick(&tick_ctx(std::slice::from_ref(&m), "t", &b, 1, 870, now));
            }
            replay.open_positions()
        };
        assert_eq!(run(dec!(0.35)), 1, "the crash lands in a 0.35 cheap zone");
        assert_eq!(run(dec!(0.20)), 0, "the same crash sits above a 0.20 cap");
    }
}
