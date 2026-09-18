//! Deterministic reference strategy logic shared by TWO loaders in the E7
//! parity harness (issue #38):
//!   - an in-tree wrapper implementing the kernel's full `EngineStrategy`, and
//!   - the external `parity_strategy` cdylib behind C ABI v2.
//!
//! There is intentionally a SINGLE copy of this algorithm: the parity test
//! drives both loaders with the same replay and requires signal-for-signal
//! identical output. If the logic here changes, both paths change together.
//!
//! The crate depends only on serde_json so it compiles identically inside the
//! core workspace and the cdylib's standalone Cargo.lock. Decimals arrive as
//! strings and are compared through a fixed-point parser (no float, no
//! rust_decimal coupling across the two lockfiles).
//!
//! The toy rule exercises every v2 capability on purpose: full-ladder depth,
//! OBI and best prices gate entries; a per-token idle/in-position state machine
//! emits exits and trend breaks; confirmation/diagnostics reflect depth; the
//! entry threshold is hot-parameter driven (`trendMaxEntryPrice`).

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const SCALE: i128 = 1_000_000_000_000_000_000; // 1e18

/// Parse a decimal string ("0.43", "100", "-1.50") to a fixed-point i128.
fn fp(s: &str) -> Option<i128> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let int_v: i128 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let mut f = frac.to_string();
    if f.len() > 18 {
        // Truncate beyond 18 fractional digits.
        f.truncate(18);
    }
    while f.len() < 18 {
        f.push('0');
    }
    let frac_v: i128 = if frac.is_empty() { 0 } else { f.parse().ok()? };
    let v = int_v.checked_mul(SCALE)?.checked_add(frac_v)?;
    Some(if neg { -v } else { v })
}

fn le(a: &str, b: &str) -> bool {
    match (fp(a), fp(b)) {
        (Some(x), Some(y)) => x <= y,
        _ => false,
    }
}
fn ge(a: &str, b: &str) -> bool {
    match (fp(a), fp(b)) {
        (Some(x), Some(y)) => x >= y,
        _ => false,
    }
}

#[derive(Clone, Debug)]
pub struct PLevel {
    pub price: String,
    pub size: String,
}

#[derive(Clone, Debug, Default)]
pub struct PBook {
    pub symbol: String,
    pub bids: Vec<PLevel>,
    pub asks: Vec<PLevel>,
    pub best_bid: String,
    pub best_ask: String,
    pub mid: String,
    pub bid_depth: String,
    pub ask_depth: String,
    pub obi: String,
    pub spread: String,
    pub spread_pct: String,
}

#[derive(Clone, Debug)]
pub struct PMarket {
    pub up_token: String,
    pub down_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PEntry {
    pub token: String,
    pub price: String,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PExit {
    pub token: String,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PBreak {
    pub token: String,
    pub broken_price: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    pub entries: Vec<PEntry>,
    pub exits: Vec<PExit>,
    pub breaks: Vec<PBreak>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    InPosition,
}

pub struct ParityStrategy {
    name: String,
    buy_below: String,
    exit_above: String,
    broken_price: String,
    min_bid_depth: String,
    books: HashMap<String, PBook>,
    phase: HashMap<String, Phase>,
    broke: HashSet<String>,
}

impl Default for ParityStrategy {
    fn default() -> Self {
        Self::new("parity")
    }
}

impl ParityStrategy {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            // Defaults deliberately match the kernel spread_arb entry ceiling so
            // the first replay dips fire; hot params can move buy_below.
            buy_below: "0.45".into(),
            exit_above: "0.60".into(),
            broken_price: "0.30".into(),
            min_bid_depth: "50".into(),
            books: HashMap::new(),
            phase: HashMap::new(),
            broke: HashSet::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Per-book callback (maps to EngineStrategy::on_book / vtable on_book).
    pub fn observe(&mut self, token: &str, book: PBook) {
        self.books.insert(token.to_string(), book);
    }

    /// Round change resets per-round state (maps to on_round).
    pub fn reset_round(&mut self, _slot: i64) {
        self.phase.clear();
        self.broke.clear();
    }

    /// Apply hot parameters. Understands the kernel MutableParams camelCase
    /// field `trendMaxEntryPrice` as the entry threshold.
    pub fn apply_hot_json(&mut self, json: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(json) else {
            return false;
        };
        if let Some(s) = v.get("trendMaxEntryPrice").and_then(|x| x.as_str())
            && fp(s).is_some()
        {
            self.buy_below = s.to_string();
            return true;
        }
        false
    }

    /// Produce this cycle's intents over the round's two-token markets. The
    /// caller passes the freshest book per token (already freshness-filtered).
    pub fn evaluate(&mut self, markets: &[PMarket]) -> Decision {
        let mut d = Decision::default();
        for m in markets {
            for token in [&m.up_token, &m.down_token] {
                let Some(b) = self.books.get(token) else {
                    continue;
                };

                // Trend break fires once per round when mid collapses.
                if le(&b.mid, &self.broken_price) && self.broke.insert(token.clone()) {
                    d.breaks.push(PBreak {
                        token: token.clone(),
                        broken_price: self.broken_price.clone(),
                    });
                }

                match self.phase.get(token).copied().unwrap_or(Phase::Idle) {
                    Phase::Idle => {
                        // Entry uses the FULL ladder: mid dip + real bid depth +
                        // non-collapsing imbalance, priced at best ask (cross).
                        let dip = le(&b.mid, &self.buy_below);
                        let deep = ge(&b.bid_depth, &self.min_bid_depth);
                        let supported = le("-0.9", &b.obi);
                        let two_sided = !b.bids.is_empty() && !b.asks.is_empty();
                        if dip && deep && supported && two_sided {
                            d.entries.push(PEntry {
                                token: token.clone(),
                                price: b.best_ask.clone(),
                                reason: "parity_dip".into(),
                            });
                            self.phase.insert(token.clone(), Phase::InPosition);
                        }
                    }
                    Phase::InPosition => {
                        // Take profit once the bid recovers to the exit level.
                        if ge(&b.best_bid, &self.exit_above) {
                            d.exits.push(PExit {
                                token: token.clone(),
                                reason: "parity_tp".into(),
                            });
                            self.phase.insert(token.clone(), Phase::Idle);
                        }
                    }
                }
            }
        }
        d
    }

    /// Confirmed = has a two-sided book with at least two levels each and a
    /// positive mid (proves the strategy sees the full ladder).
    pub fn confirmed_tokens(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .books
            .iter()
            .filter(|(_, b)| b.bids.len() >= 2 && b.asks.len() >= 2 && ge(&b.mid, "0"))
            .map(|(t, _)| t.clone())
            .collect();
        v.sort();
        v
    }

    /// One deterministic diagnostics object per known token (sorted).
    pub fn diagnostics(&self) -> Vec<Value> {
        let mut tokens: Vec<&String> = self.books.keys().collect();
        tokens.sort();
        tokens
            .iter()
            .map(|t| {
                let b = &self.books[*t];
                json!({
                    "symbol": b.symbol,
                    "bidDepth": b.bid_depth,
                    "askDepth": b.ask_depth,
                    "obi": b.obi,
                    "spreadPct": b.spread_pct,
                    "bidLevels": b.bids.len(),
                    "askLevels": b.asks.len(),
                })
            })
            .collect()
    }

    /// Self-declared config surface (a small JSON Schema fragment, human-facing).
    pub fn knobs(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "trendMaxEntryPrice": { "type": "string", "description": "dip entry ceiling" },
                "exitAbove": { "type": "string", "description": "take-profit bid" }
            }
        })
    }

    /// The knobs this strategy declares **evolvable** (E2-c / #28): names, the
    /// values in force, and the hard domain each value may never leave. This is
    /// the machine-consumed twin of `knobs()` above (which is a human-facing
    /// schema), and it is what the kernel builds counterfactual variants from.
    pub fn evolvable_knobs(&self) -> Value {
        json!({
            "knobs": [
                { "name": "trendMaxEntryPrice", "value": self.buy_below, "min": "0.05", "max": "0.90" },
                { "name": "exitAbove", "value": self.exit_above, "min": "0.05", "max": "0.99" }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(mid: &str, bid: &str, ask: &str, depth: &str, levels: usize) -> PBook {
        let lvl = |p: &str| PLevel {
            price: p.into(),
            size: depth.into(),
        };
        PBook {
            symbol: "tok".into(),
            bids: vec![lvl(bid); levels],
            asks: vec![lvl(ask); levels],
            best_bid: bid.into(),
            best_ask: ask.into(),
            mid: mid.into(),
            bid_depth: depth.into(),
            ask_depth: depth.into(),
            obi: "0".into(),
            spread: "0.02".into(),
            spread_pct: "4".into(),
        }
    }

    #[test]
    fn fixed_point_compares_decimals_without_float() {
        assert!(le("0.43", "0.45"));
        assert!(!le("0.46", "0.45"));
        assert!(ge("100.0", "50"));
        assert!(le("-0.9", "-0.5"));
        assert!(ge("0.300000000000000001", "0.3"));
    }

    #[test]
    fn dips_to_entry_then_recovers_to_exit() {
        let mut s = ParityStrategy::default();
        let m = PMarket {
            up_token: "up".into(),
            down_token: "down".into(),
        };
        s.observe("up", book("0.50", "0.49", "0.51", "100", 3));
        assert!(s.evaluate(std::slice::from_ref(&m)).entries.is_empty());
        s.observe("up", book("0.42", "0.41", "0.43", "100", 3));
        let d = s.evaluate(std::slice::from_ref(&m));
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries[0].price, "0.43");
        // No repeat entry while in position.
        assert!(s.evaluate(std::slice::from_ref(&m)).entries.is_empty());
        // Bid recovers → exit.
        s.observe("up", book("0.61", "0.61", "0.63", "100", 3));
        let d = s.evaluate(std::slice::from_ref(&m));
        assert_eq!(d.exits.len(), 1);
    }

    #[test]
    fn shallow_book_blocks_entry_but_still_confirms_with_two_levels() {
        let mut s = ParityStrategy::default();
        let m = PMarket {
            up_token: "up".into(),
            down_token: "down".into(),
        };
        s.observe("up", book("0.40", "0.39", "0.41", "10", 2));
        assert!(s.evaluate(std::slice::from_ref(&m)).entries.is_empty());
        assert_eq!(s.confirmed_tokens(), vec!["up".to_string()]);
    }

    #[test]
    fn hot_param_moves_entry_ceiling() {
        let mut s = ParityStrategy::default();
        let m = PMarket {
            up_token: "up".into(),
            down_token: "down".into(),
        };
        s.observe("up", book("0.48", "0.47", "0.49", "100", 3));
        assert!(s.evaluate(std::slice::from_ref(&m)).entries.is_empty());
        assert!(s.apply_hot_json(r#"{"trendMaxEntryPrice":"0.50"}"#));
        let d = s.evaluate(std::slice::from_ref(&m));
        assert_eq!(d.entries.len(), 1);
    }
}
