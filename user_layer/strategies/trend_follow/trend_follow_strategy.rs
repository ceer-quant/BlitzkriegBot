//! The `trend_follow` strategy (the chase leg) as a full-featured C ABI v2
//! dylib (PR-B: the kernel ships ZERO strategy code; this library enters the
//! engine through the same dlopen path any third-party strategy uses).
//!
//! Decision logic comes from `strategy-logic` — the shared reference
//! implementation crate. That is not a kernel privilege: the crate is a plain
//! dependency any strategy author can take, and taking it (or reimplementing
//! the same rules) is exactly how an external strategy reaches full parity.
//!
//! Behaviour (mirrors the reference exactly): buy the side being bid UP — its
//! own mid must have RISEN `min_move_pct` over `momentum_window_sec` and sit at
//! or above `min_confirm_price`; entry LIFTS the best ask (so entry > mid) and
//! never pays above `max_entry_price` or into a book wider than
//! `max_spread_pct`. Exits are NOT this strategy's business: the kernel's
//! shared exit policy manages any fill. The one entry-side control is the
//! break: when the chased move dies (mid back below `break_price`) the emitted
//! break intent cancels that token's resting entry bid.
//!
//! It declares NO gate exemption: a with-the-move entry passes the shared
//! spot-momentum filter naturally.
//!
//! The kernel's `on_config` package (`{"trend":…, "spreadArb":…}`) contains no
//! `trend_follow` fields, so this strategy's config arrives only through the
//! shadow-evolution hot bag (its own snake_case knob names as decimal strings)
//! — `on_params` accepts exactly that.

use blitzkrieg_strategy_api::{
    BookUpdate, Break, Entry, FreshBook, Intents, Knob, ParamBag, RoundContext, RoundInfo,
    SafeStrategy, dec, export_strategy,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
use strategy_logic::trend_follow::{MomentumTracker, apply_knobs as trend_follow_apply_knobs};
use strategy_logic::{
    OrderbookSnapshot, StrategyParams, TrendFollowConfig, evaluate_trend_follow, round2,
    trend_follow_knobs,
};

struct TrendFollow {
    cfg: TrendFollowConfig,
    tracker: MomentumTracker,
    /// Books delivered in the CURRENT evaluation context with the host's
    /// freshness verdict; entries price only off `fresh == true` rows.
    eval_books: HashMap<String, OrderbookSnapshot>,
    /// Raw book stream (all updates, fresh or not) for diagnostics.
    books: HashMap<String, BookUpdate>,
    /// The freshest host clock seen — `move_pct` needs a `now`.
    last_now_ms: i64,
}

impl Default for TrendFollow {
    fn default() -> Self {
        let cfg = TrendFollowConfig::default();
        Self {
            tracker: strategy_logic::trend_follow::MomentumTracker::new(cfg.clone()),
            cfg,
            eval_books: HashMap::new(),
            books: HashMap::new(),
            last_now_ms: 0,
        }
    }
}

impl TrendFollow {
    fn snapshot_from(&self, u: &BookUpdate) -> Option<OrderbookSnapshot> {
        let best_bid = dec(&u.best_bid)?;
        let best_ask = dec(&u.best_ask)?;
        // A book without a mid is not priceable, same as the kernel's gate.
        dec(&u.mid)?;
        let bids = if best_bid.is_sign_positive() {
            vec![(best_bid, dec(&u.bid_depth).unwrap_or(Decimal::ZERO))]
        } else {
            Vec::new()
        };
        let asks = if best_ask.is_sign_positive() {
            vec![(best_ask, dec(&u.ask_depth).unwrap_or(Decimal::ZERO))]
        } else {
            Vec::new()
        };
        Some(OrderbookSnapshot::from_levels(
            u.symbol.clone(),
            bids,
            asks,
            u.timestamp_ms,
        ))
    }

    fn apply_bag(&mut self, p: &ParamBag) {
        let mut bag = StrategyParams::new();
        for name in p.0.keys() {
            if let Some(d) = p.get_dec(name) {
                bag.set(name, d);
            }
        }
        let cfg = trend_follow_apply_knobs(&self.cfg, &bag);
        self.tracker.set_config(cfg.clone());
        self.cfg = cfg;
    }
}

impl SafeStrategy for TrendFollow {
    fn name(&self) -> &str {
        "trend_follow"
    }
    fn version(&self) -> &str {
        "0.2.0"
    }

    fn on_book(&mut self, u: &BookUpdate) {
        if u.mid.is_some() {
            self.books.insert(u.symbol.clone(), u.clone());
        }
        // Feed the tracker from the top-of-book mid, exact decimal, with the
        // host's book timestamp (= the kernel's now_ms for this update).
        if let Some(mid) = dec(&u.mid)
            && mid > Decimal::ZERO
        {
            self.tracker.on_price(&u.symbol, mid, u.timestamp_ms);
        }
        self.last_now_ms = self.last_now_ms.max(u.timestamp_ms);
    }

    fn on_round(&mut self, r: RoundInfo) {
        self.tracker.reset_if_new_round(r.slot);
        self.last_now_ms = self.last_now_ms.max(r.now_ms);
    }

    fn on_eval_books(&mut self, books: &[FreshBook]) {
        // Freshness gate: replace the priceable view wholesale each cycle. A
        // token without a fresh row this cycle is simply absent — stale and
        // missing are indistinguishable, exactly the kernel's semantics.
        self.eval_books = books
            .iter()
            .filter(|fb| fb.fresh)
            .filter_map(|fb| {
                let snap = self.snapshot_from(&fb.book)?;
                Some((fb.book.symbol.clone(), snap))
            })
            .collect();
    }

    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut it = Intents::none();
        let now = ctx.round.now_ms;
        self.last_now_ms = self.last_now_ms.max(now);
        if !self.tracker.confirmed_tokens().is_empty() {
            for m in &ctx.markets {
                if let Some(sig) = evaluate_trend_follow(
                    &m.asset,
                    &m.condition_id,
                    &m.up_token,
                    &m.down_token,
                    self.eval_books.get(&m.up_token),
                    self.eval_books.get(&m.down_token),
                    &self.tracker,
                    now,
                    &self.cfg,
                ) {
                    it.entries.push(Entry {
                        token: sig.token_id.clone(),
                        price: sig.price.to_string(),
                        reason: sig.reason.clone(),
                    });
                }
            }
        }
        // Trend breaks cancel the token's resting entry bids. Drained here so
        // each break is reported exactly once (the kernel consumes the intent).
        for b in self.take_breaks() {
            it.breaks.push(b);
        }
        it
    }

    fn take_breaks(&mut self) -> Vec<Break> {
        self.tracker
            .take_broken()
            .into_iter()
            .map(|(token, price)| Break {
                token,
                broken_price: price.to_string(),
            })
            .collect()
    }

    fn confirmed_tokens(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tracker.confirmed_tokens().into_iter().collect();
        v.sort();
        v
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        let eff = self.cfg.clone();
        let cap = eff.max_entry_price;
        let floor = eff.min_confirm_price;
        let mut tokens: Vec<String> = self.tracker.confirmed_tokens().into_iter().collect();
        tokens.sort();
        tokens
            .into_iter()
            .map(|t| {
                let book = self.eval_books.get(&t);
                let mid = book.map(|b| b.mid_price).unwrap_or(Decimal::ZERO);
                let move_pct = self.tracker.move_pct(&t, self.last_now_ms);
                let entry = book.map(|b| round2(b.best_ask)).unwrap_or(Decimal::ZERO);
                let chaseable = entry > mid && entry <= cap && mid >= floor;
                serde_json::json!({
                    "token": t, "mid": mid, "movePct": move_pct,
                    "entry": entry, "cap": cap, "chaseable": chaseable,
                })
            })
            .collect()
    }

    fn on_params(&mut self, p: &ParamBag) -> bool {
        self.apply_bag(p);
        true
    }

    fn evolvable_knobs(&self) -> Vec<Knob> {
        trend_follow_knobs(&self.cfg)
            .into_iter()
            .map(|k| Knob {
                name: k.name,
                value: k.value.to_string(),
                min: k.min.to_string(),
                max: k.max.to_string(),
            })
            .collect()
    }

    fn config_view(&self) -> Option<serde_json::Value> {
        Some(config_view_json(&self.cfg))
    }
}

fn config_view_json(cfg: &TrendFollowConfig) -> serde_json::Value {
    serde_json::json!({
        "momentumWindowSec": cfg.momentum_window_sec,
        "minMovePct": cfg.min_move_pct.to_string(),
        "minConfirmPrice": cfg.min_confirm_price.to_string(),
        "breakPrice": cfg.break_price.to_string(),
        "maxEntryPrice": cfg.max_entry_price.to_string(),
        "maxSpreadPct": cfg.max_spread_pct.to_string(),
    })
}

export_strategy!(crate::TrendFollow);
