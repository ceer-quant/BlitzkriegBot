//! The `spread_arb` strategy (the trend-confirmed dip buyer) as a full-featured
//! C ABI v2 dylib (PR-B: the kernel ships ZERO strategy code; this library
//! enters the engine through the same dlopen path any third-party strategy
//! uses).
//!
//! Decision logic comes from `strategy-logic` — the shared reference
//! implementation crate. That is not a kernel privilege: the crate is a plain
//! dependency any strategy author can take, and taking it (or reimplementing
//! the same rules) is exactly how an external strategy reaches full parity.
//!
//! ABI v2 features exercised (all optional symbols, resolved by name):
//!  - `on_eval_books`: the host-bound freshness gate — entries are priced ONLY
//!    off `fresh == true` books, identical to the kernel's `fresh_book` rule.
//!  - `on_round`: real round timing, drives the tracker's per-round reset.
//!  - `evaluate`: entry intents (resting bid below the mid) + trend breaks.
//!  - `confirmed_tokens` / `diagnostics`: kernel observability surfaces.
//!  - `on_params`: kernel config package (nested "trend"/"spreadArb") and the
//!    shadow-evolution hot bag, exact-decimal parsing.
//!  - `evolvable_knobs` + `config_view`: Shadow Evolution declarations and the
//!    config-in-force view.
//!  - `gate_exemptions`: none — the dip buyer passes both shared gates
//!    naturally.

use blitzkrieg_strategy_api::{
    BookUpdate, Break, Entry, FreshBook, Intents, Knob, ParamBag, RoundContext, RoundInfo,
    SafeStrategy, dec, export_strategy,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
use strategy_logic::{
    OrderbookSnapshot, SpreadArbConfig, TrendConfig, TrendTracker, evaluate_spread_arb,
    spread_arb_apply_knobs, spread_arb_knobs,
};

struct SpreadArb {
    trend_cfg: TrendConfig,
    cfg: SpreadArbConfig,
    trend: TrendTracker,
    /// Books delivered in the CURRENT evaluation context with the host's
    /// freshness verdict; entries price only off `fresh == true` rows.
    eval_books: HashMap<String, OrderbookSnapshot>,
    /// Raw book stream (all updates, fresh or not) for diagnostics.
    books: HashMap<String, BookUpdate>,
}

impl Default for SpreadArb {
    fn default() -> Self {
        Self {
            trend_cfg: TrendConfig::default(),
            cfg: SpreadArbConfig::default(),
            trend: TrendTracker::new(TrendConfig::default()),
            eval_books: HashMap::new(),
            books: HashMap::new(),
        }
    }
}

impl SpreadArb {
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
}

impl SafeStrategy for SpreadArb {
    fn name(&self) -> &str {
        "spread_arb"
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
            self.trend.on_price(&u.symbol, mid, u.timestamp_ms);
        }
    }

    fn on_round(&mut self, r: RoundInfo) {
        self.trend.reset_if_new_round(r.slot);
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
        let confirmed = self.trend.confirmed_tokens();
        if confirmed.is_empty() {
            return it;
        }
        for m in &ctx.markets {
            if let Some(sig) = evaluate_spread_arb(
                &m.asset,
                &m.condition_id,
                &m.up_token,
                &m.down_token,
                self.eval_books.get(&m.up_token),
                self.eval_books.get(&m.down_token),
                &confirmed,
                &self.cfg,
            ) {
                it.entries.push(Entry {
                    token: sig.token_id.clone(),
                    price: sig.price.to_string(),
                    reason: sig.reason.clone(),
                });
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
        self.trend
            .take_broken()
            .into_iter()
            .map(|(token, price)| Break {
                token,
                broken_price: price.to_string(),
            })
            .collect()
    }

    fn confirmed_tokens(&self) -> Vec<String> {
        let mut v: Vec<String> = self.trend.confirmed_tokens().into_iter().collect();
        v.sort();
        v
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        let eff = self.cfg.clone();
        let cap = eff.trend_max_entry_price;
        let floor = eff.trend_broken_price;
        let mut tokens: Vec<String> = self.trend.confirmed_tokens().into_iter().collect();
        tokens.sort();
        tokens
            .into_iter()
            .map(|t| {
                let mid = self
                    .eval_books
                    .get(&t)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                let entry = mid * eff.trend_entry_factor;
                let in_band = entry > Decimal::ZERO && entry <= cap && mid >= floor;
                serde_json::json!({
                    "token": t, "mid": mid, "entry": entry,
                    "cap": cap, "inBand": in_band,
                })
            })
            .collect()
    }

    // The kernel's config package arrives as {"trend": {...}, "spreadArb":
    // {...}}; the shadow-evolution hot bag arrives as this strategy's own knob
    // names at the top level. Both are exact-decimal overlays on the config.
    fn on_params(&mut self, p: &ParamBag) -> bool {
        let mut trend = self.trend_cfg.clone();
        let mut cfg = self.cfg.clone();
        if let Some(j) = p.get_str("trend")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(j)
        {
            if let Some(sec) = v
                .get("confirmSec")
                .and_then(|x| x.as_i64().or_else(|| x.as_str()?.parse().ok()))
            {
                trend.confirm_sec = sec;
            }
            if let Some(s) = v.get("minPrice").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                trend.min_price = d;
            }
            if let Some(s) = v.get("brokenPrice").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                trend.broken_price = d;
            }
            if let Some(s) = v.get("ratio").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                trend.ratio = d;
            }
            if let Some(ms) = v
                .get("windowFloorMs")
                .and_then(|x| x.as_i64().or_else(|| x.as_str()?.parse().ok()))
            {
                trend.window_floor_ms = ms;
            }
        }
        if let Some(j) = p.get_str("spreadArb")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(j)
        {
            if let Some(sec) = v
                .get("trendConfirmSec")
                .and_then(|x| x.as_i64().or_else(|| x.as_str()?.parse().ok()))
            {
                cfg.trend_confirm_sec = sec;
                trend.confirm_sec = sec;
            }
            if let Some(s) = v.get("trendMinPrice").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                cfg.trend_min_price = d;
                trend.min_price = d;
            }
            if let Some(s) = v.get("trendBrokenPrice").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                cfg.trend_broken_price = d;
                trend.broken_price = d;
            }
            if let Some(s) = v.get("trendEntryPrice").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                cfg.trend_entry_price = d;
            }
            if let Some(s) = v.get("trendEntryFactor").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                cfg.trend_entry_factor = d;
            }
            if let Some(s) = v.get("trendMaxEntryPrice").and_then(|x| x.as_str())
                && let Ok(d) = Decimal::from_str_exact(s)
            {
                cfg.trend_max_entry_price = d;
            }
        }
        // Hot bag (top-level snake_case knob names).
        let mut bag = strategy_logic::StrategyParams::new();
        for name in p.0.keys() {
            if let Some(d) = p.get_dec(name) {
                bag.set(name, d);
            }
        }
        cfg = spread_arb_apply_knobs(&cfg, &bag);
        if let Some(d) = bag.get("trend_min_price") {
            trend.min_price = d;
        }
        if let Some(d) = bag.get("trend_broken_price") {
            trend.broken_price = d;
        }
        self.trend_cfg = trend.clone();
        self.trend.set_config(trend);
        self.cfg = cfg;
        true
    }

    fn evolvable_knobs(&self) -> Vec<Knob> {
        spread_arb_knobs(&self.cfg)
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

fn config_view_json(cfg: &SpreadArbConfig) -> serde_json::Value {
    serde_json::json!({
        "trendMinPrice": cfg.trend_min_price.to_string(),
        "trendConfirmSec": cfg.trend_confirm_sec,
        "trendBrokenPrice": cfg.trend_broken_price.to_string(),
        "trendEntryPrice": cfg.trend_entry_price.to_string(),
        "trendEntryFactor": cfg.trend_entry_factor.to_string(),
        "trendMaxEntryPrice": cfg.trend_max_entry_price.to_string(),
    })
}

export_strategy!(crate::SpreadArb);
