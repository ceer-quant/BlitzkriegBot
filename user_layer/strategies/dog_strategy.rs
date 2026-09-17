//! KI-7 / E9 (#59): the "dog" dip strategy rewritten on the [`SafeStrategy`]
//! template — zero `unsafe` in this file; `export_strategy!` generates the
//! entire C ABI v2 surface (vtable, `#[no_mangle]` exports, string plumbing).
//! Rule: on a deep book (bid depth ≥ 50), buy when mid ≤ `trendMaxEntryPrice`
//! (default 0.43, evolvable 0.05..0.90) at the best ask («dog_dip»); while in
//! position, exit once the best bid recovers to 0.60 («dog_tp»). Entries carry
//! no size and exits no price — the kernel sizes, prices and risk-gates
//! everything; the only entry gate waived is `timing` (audited, not a bypass).

use blitzkrieg_strategy_api::{export_strategy, BookUpdate, Entry, Exit, Intents, Knob, ParamBag, RoundContext, RoundInfo, SafeStrategy};
use std::collections::{HashMap, HashSet};

struct Dog { buy_below: f64, books: HashMap<String, BookUpdate>, holding: HashSet<String> }
impl Default for Dog { fn default() -> Self { Self { buy_below: 0.43, books: Default::default(), holding: Default::default() } } }

impl SafeStrategy for Dog {
    fn name(&self) -> &str { "dog_strategy" }
    fn version(&self) -> &str { "0.2.0" }
    fn on_book(&mut self, u: &BookUpdate) {
        if u.mid.is_some() && u.best_bid.is_some() && u.best_ask.is_some() && u.bid_depth.is_some() { self.books.insert(u.symbol.clone(), u.clone()); }
    }
    fn on_round(&mut self, _r: RoundInfo) { self.holding.clear(); }
    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut it = Intents::none();
        for m in &ctx.markets { for t in [&m.up_token, &m.down_token] {
            let Some(b) = self.books.get(t) else { continue };
            if self.holding.contains(t) {
                if b.best_bid.is_some_and(|v| v >= 0.60) { it.exits.push(Exit { token: t.clone(), reason: "dog_tp".into() }); self.holding.remove(t); }
            } else if b.mid.is_some_and(|v| v <= self.buy_below) && b.bid_depth.is_some_and(|v| v >= 50.0) && b.bid_levels >= 1 && b.ask_levels >= 1 {
                it.entries.push(Entry { token: t.clone(), price: b.best_ask.unwrap(), reason: "dog_dip".into() });
                self.holding.insert(t.clone());
            }
        } }
        it
    }
    fn confirmed_tokens(&self) -> Vec<String> {
        let mut v: Vec<String> = self.books.iter().filter(|(_, b)| b.bid_levels >= 2 && b.ask_levels >= 2).map(|(t, _)| t.clone()).collect();
        v.sort(); v
    }
    fn diagnostics(&self) -> Vec<serde_json::Value> {
        let mut ks: Vec<&String> = self.books.keys().collect(); ks.sort();
        ks.iter().map(|t| { let b = &self.books[*t]; serde_json::json!({"symbol": t, "mid": b.mid, "bestBid": b.best_bid, "bestAsk": b.best_ask, "bidDepth": b.bid_depth, "holding": self.holding.contains(*t)}) }).collect()
    }
    // Both the kernel config package (nested under "spreadArb") and the hot bag
    // (the strategy's own knob cell, top-level) resolve to the same knob.
    fn on_params(&mut self, p: &ParamBag) -> bool {
        let raw = p.get_str("trendMaxEntryPrice").map(str::to_string).or_else(|| p.get_str("spreadArb").and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok()).and_then(|v| v.get("trendMaxEntryPrice").and_then(|x| x.as_str()).map(str::to_string)));
        match raw.and_then(|x| x.parse::<f64>().ok()) { Some(v) => { self.buy_below = v; true } None => false }
    }
    fn evolvable_knobs(&self) -> Vec<Knob> { vec![Knob { name: "trendMaxEntryPrice".into(), value: "0.43".into(), min: "0.05".into(), max: "0.90".into() }] }
    fn gate_exemptions(&self) -> &'static [&'static str] { &["timing"] }
}
export_strategy!(crate::Dog);
