//! The `mean_reversion` strategy (the fade leg) as a full-featured C ABI v2
//! dylib (PR-B: the kernel ships ZERO strategy code; this library enters the
//! engine through the same dlopen path any third-party strategy uses).
//!
//! Decision logic comes from `strategy-logic` — the shared reference
//! implementation crate. That is not a kernel privilege: the crate is a plain
//! dependency any strategy author can take, and taking it (or reimplementing
//! the same rules) is exactly how an external strategy reaches full parity.
//!
//! Behaviour (mirrors the reference exactly): buy a freshly CRASHED side — a
//! token whose mid FELL `min_drop_pct` off the high of its own `lookback_sec`
//! history and now sits in the cheap zone (`<= max_price`). The entry is a
//! resting bid strictly below the mid (`mid * entry_factor`, clamped down to the
//! best bid). One candidate per token per `cooldown_sec`; a book wider than
//! `max_spread_pct` is not bought. Exits are NOT this strategy's business (the
//! kernel's shared exit policy manages fills); the one entry-side control is the
//! break — once the mid recovers above `max_price` the oversold premise is gone
//! and the emitted break cancels that token's resting entry bid.
//!
//! It declares **the momentum gate exemption** (`gate_exemptions().timing`
//! stays false): the shared spot filter rejects an entry when spot moves against
//! the bet, and a crash on the bet's own asset moves spot against it by
//! construction. Every honoured exemption is recorded per order by the kernel.
//!
//! The kernel's `on_config` package (`{"trend":…, "spreadArb":…, "meanRev":…}`)
//! overlays this strategy's config through the nested `meanRev` block; the
//! shadow-evolution hot bag (its own snake_case knob names as decimal strings)
//! overlays on top — `on_params` accepts both.

use blitzkrieg_strategy_api::{
    BookUpdate, Break, Entry, FreshBook, Intents, Knob, ParamBag, RoundContext, RoundInfo,
    SafeStrategy, dec, export_strategy,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
use strategy_logic::mean_reversion::{FadeTracker, apply_knobs as mean_reversion_apply_knobs};
use strategy_logic::{
    MeanReversionConfig, OrderbookSnapshot, StrategyParams, evaluate_mean_reversion,
    mean_reversion_knobs,
};

struct MeanReversion {
    cfg: MeanReversionConfig,
    tracker: FadeTracker,
    /// Books delivered in the CURRENT evaluation context with the host's
    /// freshness verdict; entries price only off `fresh == true` rows.
    eval_books: HashMap<String, OrderbookSnapshot>,
    /// Raw book stream (all updates, fresh or not) for diagnostics.
    books: HashMap<String, BookUpdate>,
    /// The freshest host clock seen — diagnostics need a `now`.
    last_now_ms: i64,
}

impl Default for MeanReversion {
    fn default() -> Self {
        let cfg = MeanReversionConfig::default();
        Self {
            tracker: FadeTracker::new(cfg.clone()),
            cfg,
            eval_books: HashMap::new(),
            books: HashMap::new(),
            last_now_ms: 0,
        }
    }
}

impl MeanReversion {
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
        // The kernel config package arrives as {"trend":…, "spreadArb":…,
        // "meanRev":…}; the nested meanRev block is the host config in force.
        if let Some(j) = p.get_str("meanRev")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(j)
        {
            let mut pkg = StrategyParams::new();
            let key_of = |camel: &str| -> Option<&'static str> {
                Some(match camel {
                    "lookbackSec" => "lookback_sec",
                    "minDropPct" => "min_drop_pct",
                    "maxPrice" => "max_price",
                    "entryFactor" => "entry_factor",
                    "maxSpreadPct" => "max_spread_pct",
                    "cooldownSec" => "cooldown_sec",
                    "entryMinObi" => "entry_min_obi",
                    "entryBounceMinPct" => "entry_bounce_min_pct",
                    "entryBounceWindowSec" => "entry_bounce_window_sec",
                    "entryDropMaxPct" => "entry_drop_max_pct",
                    _ => return None,
                })
            };
            if let Some(obj) = v.as_object() {
                for (k, value) in obj {
                    let Some(name) = key_of(k) else { continue };
                    // Exact decimals ride as strings; integers as numbers.
                    if let Some(s) = value.as_str()
                        && let Ok(d) = Decimal::from_str_exact(s)
                    {
                        pkg.set(name, d);
                    } else if let Some(i) = value.as_i64() {
                        pkg.set(name, Decimal::from(i));
                    }
                }
            }
            let cfg = mean_reversion_apply_knobs(&self.cfg, &pkg);
            self.tracker.set_config(cfg.clone());
            self.cfg = cfg;
        }
        let mut bag = StrategyParams::new();
        for name in p.0.keys() {
            if let Some(d) = p.get_dec(name) {
                bag.set(name, d);
            }
        }
        let cfg = mean_reversion_apply_knobs(&self.cfg, &bag);
        self.tracker.set_config(cfg.clone());
        self.cfg = cfg;
    }
}

impl SafeStrategy for MeanReversion {
    fn name(&self) -> &str {
        "mean_reversion"
    }
    fn version(&self) -> &str {
        "0.3.0"
    }

    fn on_book(&mut self, u: &BookUpdate) {
        if u.mid.is_some() {
            self.books.insert(u.symbol.clone(), u.clone());
        }
        // The tracker maintains the per-token price history AND the cheap-zone
        // flag, so it must see every book, fresh or not — exactly the kernel's
        // `on_book` path.
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
        for m in &ctx.markets {
            for (token, is_up) in [(&m.up_token, true), (&m.down_token, false)] {
                if !self.eval_books.contains_key(token) {
                    continue;
                }
                if !self.tracker.is_in_zone(token) {
                    continue;
                }
                // One candidate per token per cooldown window; a refused fire
                // does NOT consume the window (the record only happens here).
                if !self.tracker.try_fire(token, now) {
                    continue;
                }
                let (up_book, down_book) = if is_up {
                    (self.eval_books.get(&m.up_token), None)
                } else {
                    (None, self.eval_books.get(&m.down_token))
                };
                if let Some(sig) = evaluate_mean_reversion(
                    &m.asset,
                    &m.condition_id,
                    &m.up_token,
                    &m.down_token,
                    up_book,
                    down_book,
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
        // Premise breaks (mid recovered above the cheap zone) cancel the
        // token's resting entry bids. Drained here so each break is reported
        // exactly once (the kernel consumes the intent).
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
        let mut v: Vec<String> = self.tracker.zone_tokens().into_iter().collect();
        v.sort();
        v
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        let eff = self.cfg.clone();
        let mut tokens: Vec<String> = self.tracker.zone_tokens().into_iter().collect();
        tokens.sort();
        tokens
            .into_iter()
            .map(|t| {
                let mid = self
                    .eval_books
                    .get(&t)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                let drop = self.tracker.drop_pct(&t, self.last_now_ms);
                let entry = mid * eff.entry_factor;
                let firable = mid > Decimal::ZERO
                    && mid <= eff.max_price
                    && drop <= -eff.min_drop_pct
                    && entry < mid;
                serde_json::json!({
                    "token": t, "mid": mid, "dropPct": drop,
                    "entry": entry, "cap": eff.max_price, "firable": firable,
                })
            })
            .collect()
    }

    fn on_params(&mut self, p: &ParamBag) -> bool {
        self.apply_bag(p);
        true
    }

    fn evolvable_knobs(&self) -> Vec<Knob> {
        mean_reversion_knobs(&self.cfg)
            .into_iter()
            .map(|k| Knob {
                name: k.name,
                value: k.value.to_string(),
                min: k.min.to_string(),
                max: k.max.to_string(),
            })
            .collect()
    }

    fn gate_exemptions(&self) -> &'static [&'static str] {
        &["momentum"]
    }

    fn config_view(&self) -> Option<serde_json::Value> {
        Some(config_view_json(&self.cfg))
    }
}

fn config_view_json(cfg: &MeanReversionConfig) -> serde_json::Value {
    serde_json::json!({
        "lookbackSec": cfg.lookback_sec,
        "minDropPct": cfg.min_drop_pct.to_string(),
        "maxPrice": cfg.max_price.to_string(),
        "entryFactor": cfg.entry_factor.to_string(),
        "maxSpreadPct": cfg.max_spread_pct.to_string(),
        "cooldownSec": cfg.cooldown_sec,
        "entryMinObi": cfg.entry_min_obi.to_string(),
        "entryBounceMinPct": cfg.entry_bounce_min_pct.to_string(),
        "entryBounceWindowSec": cfg.entry_bounce_window_sec,
        "entryDropMaxPct": cfg.entry_drop_max_pct.to_string(),
    })
}

export_strategy!(crate::MeanReversion);
