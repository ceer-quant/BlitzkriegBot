//! Wire types for the Kalshi REST surface (#423).
//!
//! Only the fields the client actually consumes are modeled; unknown fields
//! are kept (`deny_unknown_fields` is deliberately NOT used) so venue-side
//! additions never break parsing. Money is parsed into `rust_decimal::Decimal`
//! from the venue's dollar-string form.

use rust_decimal::Decimal;
use serde::Deserialize;

/// One tradeable Kalshi market inside an event.
///
/// The 15m-round (and daily) series quote in dollar-string probability fields
/// (`yes_bid_dollars: "0.65"`), NOT the legacy cent-integer fields
/// (`yes_bid: 65`) the older Kalshi surfaces document — the legacy fields are
/// absent on these series, so every quote read goes through the `_dollars`
/// forms. `0.0000` / `1.0000` are the venue's "no quote" sentinels, never
/// prices.
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub market_type: String,
    pub title: String,
    pub subtitle: Option<String>,
    /// ISO-8601 close time (trading stops).
    pub close_time: String,
    /// ISO-8601 expected settlement/expiration time.
    pub expiration_time: Option<String>,
    pub status: Option<String>,
    pub yes_bid_dollars: Option<Decimal>,
    pub yes_ask_dollars: Option<Decimal>,
    pub no_bid_dollars: Option<Decimal>,
    pub no_ask_dollars: Option<Decimal>,
    pub volume_fp: Option<Decimal>,
}

impl KalshiMarket {
    /// A market is tradeable only when the venue reports it active/open.
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_deref(), None | Some("active") | Some("open"))
    }

    /// The best quoted YES price as a 0..1 probability: the midpoint of the
    /// venue's own bid/ask, falling back to whichever side exists. `None`
    /// when the market carries no quote at all — a market that has not been
    /// quoted yet must be SKIPPED, not priced at a made-up 0.5 (the old
    /// cent-field fallback produced exactly that fake probability).
    pub fn yes_mid_prob(&self) -> Option<Decimal> {
        let in_band = |p: Decimal| p > Decimal::ZERO && p < Decimal::ONE;
        let bid = self.yes_bid_dollars.filter(|p| in_band(*p));
        let ask = self.yes_ask_dollars.filter(|p| in_band(*p));
        match (bid, ask) {
            (Some(b), Some(a)) if a >= b => Some((b + a) / Decimal::from(2)),
            (Some(b), Some(_)) => Some(b), // crossed/zero book: trust the bid side
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

/// Order book snapshot for one market (`markets/{ticker}/orderbook`).
///
/// The round series answer with the `orderbook_fp` form: two BID ladders
/// (`yes_dollars` / `no_dollars`), each entry `[price, depth]` as decimal
/// STRINGS, ascending by price (best bid = last element). The price is
/// already a 0..1 probability — no cents conversion exists anywhere on this
/// form. The depth is the resting NOTIONAL in dollars (fractional values
/// like `4467.73` prove it is not a contract count); contracts = notional /
/// price.
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderBook {
    pub orderbook_fp: Option<KalshiFpBook>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KalshiFpBook {
    #[serde(default)]
    pub yes_dollars: Vec<(String, String)>,
    #[serde(default)]
    pub no_dollars: Vec<(String, String)>,
}

/// One parsed fp ladder entry: probability price + depth converted to
/// contracts (the kernel's share unit). A level that fails to parse, sits
/// outside (0,1), or rounds to zero contracts is dropped, never forwarded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KalshiBidLevel {
    pub price: Decimal,
    pub shares: Decimal,
}

impl KalshiOrderBook {
    /// The YES bid ladder, best bid LAST (venue order preserved).
    pub fn yes_bids(&self) -> Vec<KalshiBidLevel> {
        self.fp_bids(|fp| fp.yes_dollars.as_slice())
    }

    /// The NO bid ladder, best bid LAST.
    pub fn no_bids(&self) -> Vec<KalshiBidLevel> {
        self.fp_bids(|fp| fp.no_dollars.as_slice())
    }

    fn fp_bids(&self, pick: impl Fn(&KalshiFpBook) -> &[(String, String)]) -> Vec<KalshiBidLevel> {
        let Some(fp) = self.orderbook_fp.as_ref() else {
            return Vec::new();
        };
        pick(fp)
            .iter()
            .filter_map(|(p, notional)| {
                let price: Decimal = p.parse().ok()?;
                if price <= Decimal::ZERO || price >= Decimal::ONE {
                    return None;
                }
                let notional: Decimal = notional.parse().ok()?;
                if notional <= Decimal::ZERO {
                    return None;
                }
                let shares = notional / price;
                (shares > Decimal::ZERO).then_some(KalshiBidLevel { price, shares })
            })
            .collect()
    }

    /// True only when both ladders carry at least one resting level — a
    /// one-sided book cannot be cross-platform hedged.
    pub fn is_two_sided(&self) -> bool {
        !self.yes_bids().is_empty() && !self.no_bids().is_empty()
    }
}

/// Account balance response (`portfolio/get_balance`).
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiBalance {
    pub balance: Decimal,
    pub portfolio_value: Option<Decimal>,
}

/// A resting or filled order as the venue reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrder {
    pub order_id: String,
    pub ticker: String,
    pub user_order_id: Option<String>,
    pub side: String,
    pub action: String,
    pub order_type: String,
    pub count: i64,
    pub no_count: Option<i64>,
    pub remaining_count: Option<i64>,
    pub yes_price: Option<Decimal>,
    pub no_price: Option<Decimal>,
    pub status: Option<String>,
    pub created_time: Option<String>,
}

impl KalshiOrder {
    /// Idempotency probe: did our client reference id reach a live order?
    pub fn matches_client_id(&self, client_id: &str) -> bool {
        self.user_order_id.as_deref() == Some(client_id)
    }

    /// True when the venue has stopped working this order (terminal states).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("executed") | Some("canceled") | Some("cancelled")
        )
    }
}
