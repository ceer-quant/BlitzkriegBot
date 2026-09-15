//! The built-in `trend_follow` strategy (E4-a / #30): the *chase* leg.
//!
//! Where [`super::spread_arb`] buys a dip inside a confirmed trend (resting a bid
//! strictly BELOW the mid and waiting), this strategy buys the side that is
//! being bid UP — it lifts the offer and pays above the mid. The two are
//! structural inverses, which is what makes them a hedge:
//!
//! | | `spread_arb` | `trend_follow` |
//! | --- | --- | --- |
//! | trigger | a dip inside a confirmed trend | the token's mid RISES `>= min_move_pct` |
//! | side | the underpriced side | the side being bought up (with the move) |
//! | pricing | `entry < mid`, capped at the best bid | `entry = best_ask`, so `entry > mid` |
//! | ceiling | `trend_max_entry_price` (cheap) | `max_entry_price` (payoff floor) |
//!
//! All state comes from this token's own book history (a [`PriceBuffer`] fed in
//! `on_book`), never from the scanner's cached prices — the pricing discipline
//! at the top of `signal.rs`. That keeps the backtest a replay of the live
//! decision rather than an approximation of it.
//!
//! Exits are NOT this strategy's business: it implements no exit intents, so a
//! filled position is managed by the kernel's shared exit policy
//! ([`crate::exit_policy`]) exactly like `spread_arb`'s (D-2), and nothing here
//! can reach the risk gate, kill switch, daily-loss cap, sizing ceiling or
//! quotas. Its one entry-side control is `take_breaks`: when the move it chased
//! dies (mid back below `break_price`) the kernel cancels that token's resting
//! entry bid.
//!
//! It declares **no gate exemption**: the shared spot-momentum filter rejects an
//! entry when spot moves AGAINST the bet, and a with-the-move entry passes it
//! naturally (E4-b, the counter-trend leg, is the one that needs the opt-out).
//!
//! E2-c (#28) applies unchanged: the six tunables are declared with names,
//! values and domains ([`TREND_FOLLOW_KNOBS`]) and a twin can be built from any
//! parameter set ([`TrendFollowShadowFactory`]), so a shadow variant is
//! evaluated by this strategy's own code rather than a kernel-side copy.

use super::shadow_twin::ShadowFactory;
use super::{EngineStrategy, StrategyCtx};
use crate::model::OrderbookSnapshot;
use crate::shadow_evolution::{KnobSpec, ParamRegistry, StrategyParams};
use crate::signal::{PriceBuffer, TradeSignal};
use crate::model::SignalDirection;
use arc_swap::ArcSwap;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Entry-side parameters for the chase leg. Every field is a declared knob.
///
/// The two defaults that are NOT free choices are grounded in the recorded
/// market data (`data/archive/`, ~18 h of dry-run capture), because both are
/// thresholds on a 0.01-tick binary book where "1%" can be less than one tick:
///
///  * `min_move_pct = 3.0` — at the 0.55 confirmation floor one tick is
///    `0.01/0.55 = 1.8%`, so a 1% threshold fires on a *single* uptick and is a
///    noise trigger, not a breakout detector. 3% demands a genuine multi-tick
///    rise (≈1.7 ticks at 0.55, ≈2.7 ticks at 0.90).
///  * `max_spread_pct = 3.0` — measured over the same corpus restricted to
///    `mid >= 0.55`: median quoted spread 1.4%, p90 3.1%. A 3% cap keeps ~90% of
///    observed books and refuses the fat tail; being a *relative* cap it also
///    tightens automatically as the price rises toward the payoff floor, which
///    is where a wide book does the most damage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendFollowConfig {
    /// Window (sec) over which the token's own mid move is measured.
    pub momentum_window_sec: i64,
    /// Minimum RISE (%) of the token's mid over the window to call it a breakout.
    pub min_move_pct: Decimal,
    /// The chased side must already be priced at least this high (confirmation).
    pub min_confirm_price: Decimal,
    /// Below this the move is dead and the resting entry bid is cancelled
    /// (hysteresis: strictly lower than `min_confirm_price`).
    pub break_price: Decimal,
    /// Never pay above this — the binary's payoff must still be asymmetric.
    pub max_entry_price: Decimal,
    /// Don't chase into a book this wide (%).
    pub max_spread_pct: Decimal,
}

impl Default for TrendFollowConfig {
    fn default() -> Self {
        Self {
            momentum_window_sec: 30,
            min_move_pct: dec!(3.0),
            min_confirm_price: dec!(0.55),
            break_price: dec!(0.45),
            max_entry_price: dec!(0.88),
            max_spread_pct: dec!(3.0),
        }
    }
}

/// The knobs `trend_follow` declares evolvable, as `(name, default, min, max)`.
///
/// The domain is a HARD outer bound: `guard::validate_domain` rejects anything
/// outside it before the ±gradient lock is consulted, so no number of successive
/// steps can walk a knob past these edges.
///
/// `min_move_pct`'s floor is 0.5, not 0: below that, one tick *always* exceeds
/// the threshold for any price this strategy will consider, so the knob would
/// stop being a discriminator. The ceiling of 10 is a large move for a binary.
pub const TREND_FOLLOW_KNOBS: [(&str, Decimal, Decimal, Decimal); 6] = [
    ("momentum_window_sec", dec!(30), dec!(5), dec!(300)),
    ("min_move_pct", dec!(3.0), dec!(0.5), dec!(10.0)),
    ("min_confirm_price", dec!(0.55), dec!(0.50), dec!(0.95)),
    ("break_price", dec!(0.45), dec!(0.05), dec!(0.60)),
    ("max_entry_price", dec!(0.88), dec!(0.50), dec!(0.98)),
    ("max_spread_pct", dec!(3.0), dec!(0.10), dec!(20.0)),
];

fn knob_value(cfg: &TrendFollowConfig, name: &str) -> Decimal {
    match name {
        "momentum_window_sec" => Decimal::from(cfg.momentum_window_sec),
        "min_move_pct" => cfg.min_move_pct,
        "min_confirm_price" => cfg.min_confirm_price,
        "break_price" => cfg.break_price,
        "max_entry_price" => cfg.max_entry_price,
        "max_spread_pct" => cfg.max_spread_pct,
        _ => Decimal::ZERO,
    }
}

/// This strategy's knob declaration derived from the config in force, so it
/// reports reality rather than a compiled-in default.
pub fn trend_follow_knobs(cfg: &TrendFollowConfig) -> Vec<KnobSpec> {
    TREND_FOLLOW_KNOBS
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
pub fn apply_knobs(base: &TrendFollowConfig, params: &StrategyParams) -> TrendFollowConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            // The window is a whole number of seconds in the config but travels
            // as a decimal like every other knob; truncate so a fractional
            // proposal can never produce a non-integral window.
            "momentum_window_sec" => {
                if let Some(secs) = v.trunc().to_i64() {
                    if secs > 0 {
                        cfg.momentum_window_sec = secs;
                    }
                }
            }
            "min_move_pct" => cfg.min_move_pct = v,
            "min_confirm_price" => cfg.min_confirm_price = v,
            "break_price" => cfg.break_price = v,
            "max_entry_price" => cfg.max_entry_price = v,
            "max_spread_pct" => cfg.max_spread_pct = v,
            _ => {}
        }
    }
    cfg
}

/// Per-token momentum state for the chase leg.
///
/// `on_price` feeds each token's own mid into a rolling [`PriceBuffer`] and
/// classifies it. Hysteresis mirrors `TrendTracker`: a token is CONFIRMED at or
/// above `min_confirm_price` while rising fast enough, and only BROKEN once it
/// falls below the (lower) `break_price` — so a shallow pullback inside a live
/// move does not flap the resting bid on and off.
pub struct MomentumTracker {
    cfg: TrendFollowConfig,
    buffers: HashMap<String, PriceBuffer>,
    confirmed: HashSet<String>,
    round_slot: i64,
    /// Tokens that just died; drained by the kernel to cancel their resting bids.
    broken: Vec<(String, Decimal)>,
}

/// Retention (sec) for each token's price history.
///
/// The buffer prunes by age on every push and has no resize hook, so it is sized
/// for the WIDEST window this knob can legally take — the declared domain's
/// ceiling, or the starting value when that already sits outside it. A hot swap
/// can then widen `momentum_window_sec` without the history silently shrinking
/// to a shorter span than the window it is measured over.
fn history_retention_sec(cfg: &TrendFollowConfig) -> i64 {
    let ceiling = TREND_FOLLOW_KNOBS[0].3.to_i64().unwrap_or(300);
    cfg.momentum_window_sec.max(1).max(ceiling)
}

impl MomentumTracker {
    pub fn new(cfg: TrendFollowConfig) -> Self {
        Self {
            cfg,
            buffers: HashMap::new(),
            confirmed: HashSet::new(),
            round_slot: i64::MIN,
            broken: Vec::new(),
        }
    }

    pub fn set_config(&mut self, cfg: TrendFollowConfig) {
        self.cfg = cfg;
    }

    pub fn on_price(&mut self, token_id: &str, price: Decimal, now_ms: i64) {
        if token_id.is_empty() || price <= Decimal::ZERO {
            return;
        }
        let window = self.cfg.momentum_window_sec.max(1);
        let retention = history_retention_sec(&self.cfg);
        let buf = self
            .buffers
            .entry(token_id.to_string())
            .or_insert_with(|| PriceBuffer::new(retention));
        buf.push(price, now_ms);

        if self.confirmed.contains(token_id) {
            // Regime change: the move that justified the chase is dead.
            if self.cfg.break_price > Decimal::ZERO && price < self.cfg.break_price {
                self.confirmed.remove(token_id);
                self.broken.push((token_id.to_string(), price));
            }
            return;
        }

        let move_pct = buf.move_pct(window, now_ms);
        if price >= self.cfg.min_confirm_price && move_pct >= self.cfg.min_move_pct {
            self.confirmed.insert(token_id.to_string());
        }
    }

    pub fn reset_if_new_round(&mut self, slot: i64) {
        if slot != self.round_slot {
            self.round_slot = slot;
            self.buffers.clear();
            self.confirmed.clear();
        }
    }

    pub fn is_confirmed(&self, token_id: &str) -> bool {
        self.confirmed.contains(token_id)
    }

    pub fn confirmed_tokens(&self) -> HashSet<String> {
        self.confirmed.clone()
    }

    /// The move (%) currently measured for a token, for the entry reason string.
    pub fn move_pct(&self, token_id: &str, now_ms: i64) -> Decimal {
        let window = self.cfg.momentum_window_sec.max(1);
        self.buffers.get(token_id).map(|b| b.move_pct(window, now_ms)).unwrap_or(Decimal::ZERO)
    }

    /// Drain tokens whose move just died (the kernel cancels their bids).
    pub fn take_broken(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.broken)
    }
}

/// Trend-following (chase) evaluator for one market's two outcome tokens.
///
/// Returns a signal on the side that is being bid up: its own mid must have
/// RISEN at least `min_move_pct` over the window and sit at or above
/// `min_confirm_price` (the tracker's job), the book must be fresh, two-sided
/// and no wider than `max_spread_pct`, and the lifted offer (`best_ask`, rounded
/// to the tick) must not exceed `max_entry_price`.
///
/// The pricing rule is the structural inverse of `evaluate_spread_arb`'s: this
/// one requires `entry > mid` (pay up) where that one requires `entry < mid`
/// (wait for a dip).
pub fn evaluate_trend_follow(
    asset: &str,
    condition_id: &str,
    up_token: &str,
    down_token: &str,
    up_book: Option<&OrderbookSnapshot>,
    down_book: Option<&OrderbookSnapshot>,
    tracker: &MomentumTracker,
    now_ms: i64,
    cfg: &TrendFollowConfig,
) -> Option<TradeSignal> {
    for (token, dir, book) in [
        (up_token, SignalDirection::Up, up_book),
        (down_token, SignalDirection::Down, down_book),
    ] {
        if token.is_empty() || !tracker.is_confirmed(token) {
            continue;
        }
        let Some(book) = book else { continue };
        if book.bids.is_empty() || book.asks.is_empty() || book.mid_price <= Decimal::ZERO {
            continue;
        }
        // Don't chase into a wide book: paying a fat spread is the whole edge.
        if cfg.max_spread_pct > Decimal::ZERO && book.spread_pct > cfg.max_spread_pct {
            continue;
        }
        let mid = book.mid_price;
        // Lift the offer. A book whose ask is at or below the mid is degenerate
        // (crossed/one-sided) and is not something to chase.
        let entry = round2(book.best_ask);
        if entry <= mid || entry <= Decimal::ZERO {
            continue;
        }
        if entry > cfg.max_entry_price {
            continue;
        }
        let move_pct = tracker.move_pct(token, now_ms);
        return Some(TradeSignal {
            strategy: "trend_follow".into(),
            asset: asset.to_string(),
            direction: dir,
            token_id: token.to_string(),
            condition_id: condition_id.to_string(),
            price: entry,
            reason: format!(
                "{} rising {:+.2}% in {}s (mid {} >= {}), lifted offer {} (cap {})",
                dir.as_str().to_uppercase(),
                move_pct,
                cfg.momentum_window_sec,
                mid,
                cfg.min_confirm_price,
                entry,
                cfg.max_entry_price
            ),
        });
    }
    None
}

fn round2(v: Decimal) -> Decimal {
    (v * Decimal::ONE_HUNDRED).round() / Decimal::ONE_HUNDRED
}

pub struct TrendFollowBuiltin {
    cfg: TrendFollowConfig,
    tracker: MomentumTracker,
    /// This strategy's OWN cell in the host's per-strategy parameter registry
    /// (E2-c). None = evolution inactive, so the base config stands alone.
    hot_params: Option<Arc<ArcSwap<StrategyParams>>>,
}

impl TrendFollowBuiltin {
    pub fn new(cfg: TrendFollowConfig) -> Self {
        Self { tracker: MomentumTracker::new(cfg.clone()), cfg, hot_params: None }
    }

    /// The config currently in force: base overlaid with the hot-swapped
    /// per-strategy parameters when Shadow Evolution is active.
    fn effective_cfg(&self) -> TrendFollowConfig {
        match &self.hot_params {
            Some(h) => apply_knobs(&self.cfg, &h.load()),
            None => self.cfg.clone(),
        }
    }

    /// Push the in-force config into this instance's OWN tracker, so a hot-swap
    /// of `min_move_pct`/`min_confirm_price`/`break_price` changes what it
    /// confirms — not just the entry reason string.
    ///
    /// Applied unconditionally, including after `set_hot_params(None)`: detaching
    /// must restore the base config in the tracker too, or the last overlay would
    /// keep governing confirmation after evolution was switched off.
    fn sync_tracker_cfg(&mut self) {
        let cfg = self.effective_cfg();
        self.tracker.set_config(cfg);
    }
}

impl EngineStrategy for TrendFollowBuiltin {
    fn name(&self) -> &str {
        "trend_follow"
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64) {
        self.sync_tracker_cfg();
        self.tracker.on_price(token_id, snap.mid_price, now_ms);
    }

    fn on_round(&mut self, slot: i64) {
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
            if let Some(sig) = evaluate_trend_follow(
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
        self.tracker
            .confirmed_tokens()
            .into_iter()
            .map(|t| {
                let mid = ctx.fresh_book(&t).map(|b| b.mid_price).unwrap_or(Decimal::ZERO);
                let move_pct = self.tracker.move_pct(&t, ctx.now_ms());
                let entry = ctx.fresh_book(&t).map(|b| round2(b.best_ask)).unwrap_or(Decimal::ZERO);
                let chaseable = entry > mid
                    && entry <= eff.max_entry_price
                    && mid >= eff.min_confirm_price;
                serde_json::json!({
                    "token": t,
                    "mid": mid,
                    "movePct": move_pct,
                    "entry": entry,
                    "cap": eff.max_entry_price,
                    "chaseable": chaseable,
                })
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
        trend_follow_knobs(&self.effective_cfg())
    }

    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        // Built from the config in force WITHOUT the hot overlay: the overlay is
        // exactly what the factory's parameter argument applies.
        Some(Box::new(TrendFollowShadowFactory { base: self.cfg.clone() }))
    }
}

/// Builds independent `trend_follow` twins for the shadow engine.
///
/// A twin owns its OWN [`MomentumTracker`], so `min_move_pct`,
/// `min_confirm_price` and `break_price` genuinely change what the twin confirms
/// and breaks — a counterfactual, not a re-labelled baseline.
pub struct TrendFollowShadowFactory {
    base: TrendFollowConfig,
}

impl ShadowFactory for TrendFollowShadowFactory {
    fn strategy(&self) -> String {
        "trend_follow".to_string()
    }

    fn knobs(&self) -> Vec<KnobSpec> {
        trend_follow_knobs(&self.base)
    }

    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
        Some(Box::new(TrendFollowBuiltin::new(apply_knobs(&self.base, params))))
    }
}

#[cfg(test)]
mod tests {
    use super::super::shadow_twin::{tick_ctx, TwinReplay};
    use super::*;
    use crate::exit_policy::ExitConfig;
    use crate::model::CryptoMarket;
    use crate::shadow_evolution::variants::Metrics;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        use rust_decimal::prelude::FromPrimitive;
        book_d(Decimal::from_f64(bid).unwrap(), Decimal::from_f64(ask).unwrap())
    }

    fn book_d(bid: Decimal, ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels(
            "t",
            vec![(bid, dec!(100))],
            vec![(ask, dec!(100))],
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

    /// Drive a RISE from `from` to `to` on `token` in `count` steps, then return
    /// the clock. The move is read from the token's own buffer, so two samples
    /// far enough apart already span it.
    fn rise(t: &mut MomentumTracker, token: &str, from: Decimal, to: Decimal, steps: usize, start: i64) -> i64 {
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
        let k = trend_follow_knobs(&TrendFollowConfig::default());
        assert_eq!(k.len(), 6);
        for spec in &k {
            assert!(spec.is_coherent(), "{spec:?}");
        }
        assert_eq!(k[0].name, "momentum_window_sec");
        assert_eq!(k[0].value, dec!(30));
        let cap = k.iter().find(|s| s.name == "max_entry_price").unwrap();
        assert_eq!(cap.value, dec!(0.88));
        assert_eq!(cap.max, dec!(0.98));
        // An out-of-domain starting config widens the declared domain instead of
        // declaring a box the strategy cannot fit in.
        let cfg = TrendFollowConfig { momentum_window_sec: 900, ..Default::default() };
        let k = trend_follow_knobs(&cfg);
        let win = k.iter().find(|s| s.name == "momentum_window_sec").unwrap();
        assert!(win.contains(dec!(900)), "{win:?}");
        assert!(win.is_coherent());
    }

    #[test]
    fn apply_knobs_ignores_undeclared_names_and_keeps_the_window_integral() {
        let base = TrendFollowConfig::default();
        let mut p = StrategyParams::new();
        p.set("max_entry_price", dec!(0.80));
        p.set("momentum_window_sec", dec!(45.7));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.max_entry_price, dec!(0.80));
        assert_eq!(out.momentum_window_sec, 45, "a fractional window truncates");
        assert_eq!(out.min_move_pct, base.min_move_pct);
        // A non-positive window can never be written through the overlay.
        let mut bad = StrategyParams::new();
        bad.set("momentum_window_sec", dec!(0));
        assert_eq!(apply_knobs(&base, &bad).momentum_window_sec, base.momentum_window_sec);
    }

    #[test]
    fn a_rise_confirms_and_a_fall_below_the_break_price_cancels() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        // Flat: no move → not confirmed.
        for i in 0..5 {
            t.on_price("t", dec!(0.60), i * 1_000);
        }
        assert!(!t.is_confirmed("t"), "a flat mid is not a breakout");
        // Rise 0.50 → 0.62 (+24%) → confirmed.
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 10_000);
        assert!(t.is_confirmed("t"));
        assert!(t.confirmed_tokens().contains("t"));
        // Shallow pullback stays confirmed (hysteresis: break_price 0.45 < 0.55).
        t.on_price("t", dec!(0.50), now + 1_000);
        assert!(t.is_confirmed("t"), "a pullback inside the band must not flap");
        assert!(t.take_broken().is_empty());
        // Below the break price → broken, drained exactly once.
        t.on_price("t", dec!(0.40), now + 2_000);
        assert!(!t.is_confirmed("t"));
        let broken = t.take_broken();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "t");
        assert_eq!(broken[0].1, dec!(0.40));
        assert!(t.take_broken().is_empty(), "each break is returned once");
    }

    #[test]
    fn a_new_round_resets_the_chase_state() {
        let mut t = MomentumTracker::new(TrendFollowConfig::default());
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 0);
        assert!(t.is_confirmed("t"));
        t.on_price("t", dec!(0.70), now + 1_000);
        t.reset_if_new_round(2);
        assert!(!t.is_confirmed("t"), "a new round starts flat");
        assert_eq!(t.move_pct("t", now + 2_000), Decimal::ZERO);
    }

    #[test]
    fn the_entry_lifts_the_offer_and_is_the_inverse_of_the_dip_buyer() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 0);
        // mid 0.625, ask 0.63 → entry 0.63, strictly ABOVE the mid: paying up.
        let b = book(0.62, 0.63);
        let sig = evaluate_trend_follow(
            "BTC", "c", "t", "t-down", Some(&b), None, &t, now, &cfg,
        )
        .expect("a rising, favoured, tight book is chaseable");
        assert_eq!(sig.strategy, "trend_follow");
        assert_eq!(sig.direction, SignalDirection::Up);
        assert_eq!(sig.price, dec!(0.63));
        assert!(sig.price > b.mid_price, "{} must be above mid {}", sig.price, b.mid_price);

        // The same book through spread_arb's rule is rejected: its entry is
        // capped at the best bid, which is BELOW mid.
        let mut confirmed = HashSet::new();
        confirmed.insert("t".to_string());
        let arb = crate::signal::evaluate_spread_arb(
            "BTC", "c", "t", "t-down", Some(&b), None, &confirmed,
            &crate::signal::SpreadArbConfig::default(),
        );
        assert!(arb.is_none(), "the dip buyer must not chase this book");
    }

    #[test]
    fn an_unconfirmed_or_untraded_side_produces_nothing() {
        let cfg = TrendFollowConfig::default();
        let t = MomentumTracker::new(cfg.clone());
        let b = book(0.62, 0.63);
        // Nothing confirmed yet.
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", Some(&b), None, &t, 0, &cfg).is_none());
        // Confirmed but no fresh book for that token.
        let mut t2 = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t2, "t", dec!(0.50), dec!(0.62), 5, 0);
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", None, None, &t2, now, &cfg).is_none());
    }

    #[test]
    fn a_wide_book_is_not_chased() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 0);
        // mid 0.60, spread 0.04 → 6.7% > max_spread_pct 3.0 → refuse to chase.
        let wide = book(0.58, 0.62);
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", Some(&wide), None, &t, now, &cfg).is_none());
        // Tighten the cap and the same book becomes acceptable — the knob is the
        // only thing that changed.
        let loose = TrendFollowConfig { max_spread_pct: dec!(10), ..cfg.clone() };
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", Some(&wide), None, &t, now, &loose).is_some());
    }

    #[test]
    fn the_payoff_floor_refuses_to_chase_into_no_edge() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t, "t", dec!(0.80), dec!(0.92), 5, 0);
        // ask 0.93 > max_entry_price 0.88 → paying that leaves ~7% for a 100% risk.
        let rich = book(0.92, 0.93);
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", Some(&rich), None, &t, now, &cfg).is_none());
        // A book inside the cap is taken.
        let ok = book(0.85, 0.86);
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", Some(&ok), None, &t, now, &cfg).is_some());
    }

    #[test]
    fn a_lower_entry_cap_is_a_real_counterfactual_in_the_same_tick_stream() {
        let m = market();
        let run = |cap: Decimal| -> usize {
            let base = TrendFollowConfig { max_entry_price: cap, ..Default::default() };
            let strat = TrendFollowBuiltin::new(base);
            let factory = strat.shadow_factory().unwrap();
            let mut params = StrategyParams::from_knobs(&factory.knobs());
            params.set("max_entry_price", cap);
            let twin = factory.make(&params).unwrap();
            let mut replay = TwinReplay::new(twin, &ExitConfig::default());
            replay.on_round(&[m.clone()], &[], 0);
            let mut now = 0;
            // Flat at 0.50 (below the confirmation floor, so nothing is armed),
            // then ONE jump to a 0.86 offer: the breakout is confirmed and priced
            // in the same tick, so the cap alone decides.
            for _ in 0..3 {
                now += 1_000;
                let flat = book(0.49, 0.50);
                replay.on_tick(&tick_ctx(&[m.clone()], "t", &flat, 1, 880, now));
            }
            now += 1_000;
            let jump = book_d(dec!(0.85), dec!(0.86));
            replay.on_tick(&tick_ctx(&[m.clone()], "t", &jump, 1, 880, now));
            replay.closed_trades() + replay.open_positions()
        };
        assert_eq!(run(dec!(0.90)), 1, "the lifted offer 0.86 is inside a 0.90 cap");
        assert_eq!(run(dec!(0.80)), 0, "the same offer is outside a 0.80 cap");
    }

    #[test]
    fn twin_runs_the_strategys_own_logic_and_exits_on_the_shared_policy() {
        let strat = TrendFollowBuiltin::new(TrendFollowConfig::default());
        let factory = strat.shadow_factory().expect("trend_follow is evolvable");
        assert_eq!(factory.strategy(), "trend_follow");
        let params = StrategyParams::from_knobs(&factory.knobs());
        let twin = factory.make(&params).expect("twin builds");
        let mut replay = TwinReplay::new(twin, &ExitConfig::default());

        let m = market();
        replay.on_round(&[m.clone()], &[], 0);
        // Flat start: nothing to chase.
        let mut now = 0;
        for _ in 0..3 {
            now += 1_000;
            let flat = book(0.50, 0.51);
            replay.on_tick(&tick_ctx(&[m.clone()], "t", &flat, 1, 880, now));
        }
        assert_eq!(replay.open_positions(), 0, "a flat book is not a breakout");

        // Breakout: 0.50 → 0.62, offer 0.63 → the strategy's own entry fires.
        // Half-cent touch: a 2-cent book at mid 0.62 is a 3.2% spread and would be
        // refused by the default cap, which is not what this test is about.
        for i in 1..=5 {
            now += 1_000;
            let mid = dec!(0.50) + (dec!(0.12) * Decimal::from(i as u32) / dec!(5));
            let b = book_d(mid - dec!(0.005), mid + dec!(0.005));
            replay.on_tick(&tick_ctx(&[m.clone()], "t", &b, 1, 880, now));
        }
        assert_eq!(replay.open_positions(), 1, "twin must enter via its own logic");

        // Run-up → the shared exit policy is now armed (a >50% high) but rides the
        // winner: a chase entry at ~0.58 can never reach the 100% fixed
        // take-profit, so the trailing stop is the mechanism that must pay here.
        now += 1_000;
        let up = book(0.88, 0.90);
        replay.on_tick(&tick_ctx(&[m.clone()], "t", &up, 1, 870, now));
        assert_eq!(replay.open_positions(), 1, "the policy rides a rising position");

        // Give back a third of the move → the trailing stop takes the profit.
        now += 1_000;
        let back = book(0.70, 0.72);
        replay.on_tick(&tick_ctx(&[m.clone()], "t", &back, 1, 860, now));
        assert_eq!(replay.open_positions(), 0);
        let metrics = Metrics::from_trades(&replay.windowed_trades(1800, now));
        assert_eq!(metrics.sample_count, 1);
        assert_eq!(metrics.wins, 1);
        assert!(metrics.total_pnl > Decimal::ZERO, "expected a profit, got {}", metrics.total_pnl);
    }

    #[test]
    fn a_higher_move_threshold_skips_the_same_breakout() {
        // Same tick stream, one knob moved, a different decision — produced by the
        // strategy's own code path in both cases.
        let m = market();
        let run = |need: Decimal| -> usize {
            let base = TrendFollowConfig { min_move_pct: need, ..Default::default() };
            let strat = TrendFollowBuiltin::new(base);
            let factory = strat.shadow_factory().unwrap();
            let mut params = StrategyParams::from_knobs(&factory.knobs());
            params.set("min_move_pct", need);
            let twin = factory.make(&params).unwrap();
            let mut replay = TwinReplay::new(twin, &ExitConfig::default());
            replay.on_round(&[m.clone()], &[], 0);
            let mut now = 0;
            // +4.2% over the window (0.60 → 0.625), already above the 0.55 floor.
            for i in 0..5 {
                let mid = dec!(0.60) + (dec!(0.025) * Decimal::from(i as u32) / dec!(4));
                now += 1_000;
                let b = book_d(mid - dec!(0.005), mid + dec!(0.005));
                replay.on_tick(&tick_ctx(&[m.clone()], "t", &b, 1, 880, now));
            }
            replay.closed_trades() + replay.open_positions()
        };
        assert_eq!(run(dec!(1.0)), 1, "+4.2% clears a 1% threshold");
        assert_eq!(run(dec!(5.0)), 0, "the same +4.2% misses a 5% threshold");
    }

    #[test]
    fn a_hot_swapped_knob_drives_the_tracker_and_detaching_restores_the_base() {
        use crate::shadow_evolution::ParamRegistry;
        let base = TrendFollowConfig::default();
        let mut s = TrendFollowBuiltin::new(base.clone());
        let reg = ParamRegistry::new();
        let mut p = StrategyParams::new();
        // +3.6% over the window: above the base 3.0% threshold, below a 10% one.
        p.set("min_move_pct", dec!(10));
        reg.publish("trend_follow", p);
        s.set_hot_params(Some(Arc::new(reg)));

        let rise = |s: &mut TrendFollowBuiltin, token: &str| {
            for i in 0..5 {
                let mid = dec!(0.55) + (dec!(0.02) * Decimal::from(i as u32) / dec!(4));
                s.on_book(token, &book_d(mid - dec!(0.005), mid + dec!(0.005)), i as i64 * 1_000);
            }
        };
        rise(&mut s, "hot");
        assert!(
            !s.confirmed_tokens().contains("hot"),
            "a +3.6% move must miss the hot-swapped 10% threshold"
        );

        // Detach the overlay → the base config stands alone again, and the same
        // tick stream now confirms.
        s.set_hot_params(None);
        assert_eq!(s.effective_cfg(), base);
        rise(&mut s, "base");
        assert!(
            s.confirmed_tokens().contains("base"),
            "with evolution detached the base 3.0% threshold is what applies"
        );
    }
}
