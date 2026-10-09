//! Wire types for the predict.fun REST surface (#423).
//!
//! Only the fields the client actually consumes are modeled; unknown fields
//! are kept (`deny_unknown_fields` is deliberately NOT used) so venue-side
//! additions never break parsing. Prices are 0~1 probability decimals and
//! shares are venue strings — both parsed into `rust_decimal::Decimal` so
//! no float ever rounds an order price.

use rust_decimal::Decimal;
use serde::Deserialize;

/// One tradeable predict.fun market (outcome token) as `/v1/markets` reports
/// it. A multi-outcome event surfaces as several rows that share
/// `condition_id`/`event_id` — stage 2's discovery folds them.
#[derive(Debug, Clone, Deserialize)]
pub struct PredictMarket {
    pub token_id: String,
    pub question: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub condition_id: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub market_slug: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    /// ISO-8601 end of trading.
    #[serde(default)]
    pub end_date: Option<String>,
    /// Negative-risk (multi-outcome) markets route through a different
    /// exchange contract — the executor needs the flag at placement time.
    #[serde(default)]
    pub is_neg_risk: bool,
    #[serde(default)]
    pub is_yield_bearing: bool,
    /// Venue fee for this market in basis points (taker side; makers free).
    #[serde(default)]
    pub fee_rate_bps: u64,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub best_bid: Option<Decimal>,
    #[serde(default)]
    pub best_ask: Option<Decimal>,
    #[serde(default)]
    pub volume_24h: Option<Decimal>,
    #[serde(default)]
    pub liquidity_24h: Option<Decimal>,
}

impl PredictMarket {
    /// A market is tradeable only when the venue reports it active/open;
    /// an absent status field is treated as live (the venue omits it on the
    /// list endpoint but always sends it once a market halts).
    pub fn is_active(&self) -> bool {
        matches!(
            self.status.as_deref(),
            None | Some("ACTIVE") | Some("active") | Some("open") | Some("OPEN")
        )
    }

    /// 0~1 mid from the venue's own best bid/ask, when both exist. Cross-
    /// platform mapping (stage 2) compares these, not last-trade prints.
    pub fn mid(&self) -> Option<Decimal> {
        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) if b > Decimal::ZERO && a > b => Some((b + a) / Decimal::from(2)),
            _ => None,
        }
    }
}

/// A price level as predict.fun quotes it: 0~1 price string plus share depth.
#[derive(Debug, Clone, Deserialize)]
pub struct PredictLevel {
    pub price: Decimal,
    pub shares: Decimal,
}

/// Order book snapshot for one token (`/v1/markets/{id}/orderbook`).
#[derive(Debug, Clone, Deserialize)]
pub struct PredictOrderBook {
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default)]
    pub bids: Vec<PredictLevel>,
    #[serde(default)]
    pub asks: Vec<PredictLevel>,
}

impl PredictOrderBook {
    pub fn best_bid(&self) -> Option<&PredictLevel> {
        self.bids.first()
    }

    pub fn best_ask(&self) -> Option<&PredictLevel> {
        self.asks.first()
    }

    /// True only when both sides carry at least one resting level — a
    /// one-sided book cannot be cross-platform hedged.
    pub fn is_two_sided(&self) -> bool {
        !self.bids.is_empty() && !self.asks.is_empty()
    }
}

/// Account balance response (`/v1/balance`).
#[derive(Debug, Clone, Deserialize)]
pub struct PredictBalance {
    /// Collateral available for new orders (USDT).
    pub balance: Decimal,
    #[serde(default)]
    pub total_value: Option<Decimal>,
}

/// One outcome-token position as `/v1/positions` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct PredictPosition {
    pub token_id: String,
    /// Shares held on the YES side (venue reports yes/no buckets separately).
    #[serde(default)]
    pub yes_amount: Decimal,
    #[serde(default)]
    pub no_amount: Decimal,
    #[serde(default)]
    pub avg_entry_price: Option<Decimal>,
    #[serde(default)]
    pub current_price: Option<Decimal>,
}

impl PredictPosition {
    /// Net share exposure signed by side: positive = long YES, negative =
    /// long NO. A flat position (both buckets empty) reads as zero.
    pub fn signed_shares(&self) -> Decimal {
        self.yes_amount - self.no_amount
    }
}

/// A resting or filled order as the venue reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct PredictOrder {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub order_hash: Option<String>,
    pub token_id: String,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub order_type: Option<String>,
    pub price: Decimal,
    pub shares: Decimal,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub timestamp: Option<i64>,
}

impl PredictOrder {
    /// Idempotency probe: did our client order hash reach a live order?
    /// predict.fun has no client order id, so the EIP-712 order hash is the
    /// dedup key the submit path reconciles against.
    pub fn matches_hash(&self, order_hash: &str) -> bool {
        self.order_hash.as_deref() == Some(order_hash)
    }

    /// True when the venue has stopped working this order (terminal states).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("FILLED") | Some("CANCELED") | Some("CANCELLED")
        )
    }
}
