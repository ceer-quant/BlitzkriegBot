//! Wire types for the Kalshi REST surface (#423).
//!
//! Only the fields the client actually consumes are modeled; unknown fields
//! are kept (`deny_unknown_fields` is deliberately NOT used) so venue-side
//! additions never break parsing. Money is parsed into `rust_decimal::Decimal`
//! from the venue's dollar-string form.

use rust_decimal::Decimal;
use serde::Deserialize;

/// One tradeable Kalshi market inside an event.
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub market_type: String,
    pub title: String,
    pub subtitle: Option<String>,
    /// Settlement source, e.g. a reference to the underlying series.
    pub settlement_source: Option<String>,
    /// ISO-8601 close time.
    pub close_time: String,
    /// ISO-8601 expected settlement/expiration time.
    pub expiration_time: Option<String>,
    /// `true` while the market accepts new orders.
    pub open_interest: Option<i64>,
    pub status: Option<String>,
    pub yes_bid: Option<Decimal>,
    pub yes_ask: Option<Decimal>,
    pub no_bid: Option<Decimal>,
    pub no_ask: Option<Decimal>,
    pub last_price: Option<Decimal>,
    pub volume: Option<i64>,
    pub liquidity: Option<i64>,
}

impl KalshiMarket {
    /// A market is tradeable only when the venue reports it active/open.
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_deref(), None | Some("active") | Some("open"))
    }
}

/// A price level as Kalshi quotes it: cents per contract plus side depth.
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiLevel {
    pub price: Decimal,
    pub quantity: i64,
}

impl KalshiLevel {
    pub fn shares(&self) -> Decimal {
        Decimal::from(self.quantity)
    }
}

/// Order book snapshot for one market (exchange client variant `market/orderbook`).
#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderBook {
    pub orderbook: Option<KalshiOrderBookBody>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KalshiOrderBookBody {
    pub yes: Option<Vec<KalshiLevel>>,
    pub no: Option<Vec<KalshiLevel>>,
}

impl KalshiOrderBook {
    pub fn yes_levels(&self) -> &[KalshiLevel] {
        self.orderbook
            .as_ref()
            .and_then(|b| b.yes.as_deref())
            .unwrap_or(&[])
    }

    pub fn no_levels(&self) -> &[KalshiLevel] {
        self.orderbook
            .as_ref()
            .and_then(|b| b.no.as_deref())
            .unwrap_or(&[])
    }

    /// True only when both sides carry at least one resting level — a
    /// one-sided book cannot be cross-platform hedged.
    pub fn is_two_sided(&self) -> bool {
        !self.yes_levels().is_empty() && !self.no_levels().is_empty()
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
