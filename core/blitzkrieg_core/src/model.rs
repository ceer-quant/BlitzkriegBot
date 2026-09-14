//! Core domain model: orders, fills, order states, structured error codes.
//!
//! The market-agnostic primitives (`Side`, `FillPolicy`, `OrderStatus`, …) now
//! live in `blitzkrieg-market-api` and are re-exported here, so the core and
//! every market plugin share ONE definition (P0.6). This module keeps only the
//! core-owned structs. Nothing here performs I/O.

use crate::decimal;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// Shared market-agnostic primitives (single source of truth = market_api).
pub use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, FillPolicy, FillStatus, OrderId, OrderStatus, OrderType,
    Side, TokenId, TradeId,
};

/// Runtime mode. Core-owned (the market contract does not have a notion of dry
/// vs live; each plugin implements both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Simulated: no exchange calls; fills synthesised from market books.
    Dry,
    /// Real orders via the CLOB.
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    pub token_id: TokenId,
    pub condition_id: String,
    pub side: Side,
    pub mode: FillPolicy,
    #[serde(with = "decimal")]
    pub price: Decimal,
    #[serde(with = "decimal")]
    pub size: Decimal,
    /// Strategy/asset/direction/slot provenance, echoed back on events.
    pub internal_key: String,
    pub strategy: String,
    pub asset: String,
    #[serde(rename = "direction")]
    pub direction: String,
    pub round_slot: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub order_id: OrderId,
    pub trade_id: Option<TradeId>,
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "decimal")]
    pub price: Decimal,
    /// Cumulative filled size reported by this event (the OME derives deltas).
    #[serde(with = "decimal")]
    pub size: Decimal,
    pub status: FillStatus,
    /// Exchange-reported timestamp (ms).
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
}

/// A core-side tracked order. Mirrors the Node TrackedOrder but is authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedOrder {
    pub order_id: OrderId,
    pub internal_key: String,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub token_id: TokenId,
    pub condition_id: String,
    pub side: Side,
    pub mode: FillPolicy,
    #[serde(with = "decimal")]
    pub price: Decimal,
    #[serde(with = "decimal")]
    pub size: Decimal,
    #[serde(with = "decimal")]
    pub filled_size: Decimal,
    #[serde(with = "decimal::opt", default)]
    pub avg_fill_price: Option<Decimal>,
    pub status: OrderStatus,
    pub round_slot: i64,
    pub submitted_at_ms: i64,
    pub updated_at_ms: i64,
    /// Exchange order id once the venue acknowledges a LIVE order (None in dry,
    /// or before the POST resolves). Reconciliation matches WS/REST on this.
    #[serde(default)]
    pub venue_order_id: Option<String>,
    /// Pending simulated maker→taker escalation deadline (ms), dry mode only.
    #[serde(default)]
    pub escalate_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    OrderUpdate,
    Fill,
    RiskAlert,
    Reconcile,
    Ready,
    Error,
}

// ── Exit reasons & orderbook ─────────────────────────────────────────────────

/// Which side of a binary market a position/signal is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalDirection {
    Up,
    Down,
}

/// Why a position was closed. Mirrors the TS `ExitReason` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    RatchetFloor,
    TrailingStop,
    BreakevenLock,
    DepthCollapse,
    StaleProfit,
    StagnantProfit,
    TimeExit,
    ForceExit,
    SpotReversal,
    QuickProfit,
    Manual,
    /// Explicit close requested by a hosted (in-tree or external) strategy via
    /// an exit *intent*. The kernel still prices, sizes, risk-checks and
    /// submits it — the strategy never places anything itself.
    StrategySignal,
}

/// A binary UP/DOWN market for one asset in the current round.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CryptoMarket {
    pub asset: String,
    pub condition_id: String,
    pub question_id: String,
    pub up_token_id: TokenId,
    pub down_token_id: TokenId,
    #[serde(with = "decimal")]
    pub up_price: Decimal,
    #[serde(with = "decimal")]
    pub down_price: Decimal,
    pub expires_at_ms: i64,
    pub round_slot: i64,
    pub neg_risk: bool,
    pub question: String,
}

/// A snapshot of the top-of-book plus derived depth/obi metrics. Mirrors the TS
/// `OrderbookSnapshot`. Carried on the tick/decide path; the book itself is
/// rebuilt by the market-data layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderbookSnapshot {
    pub token_id: TokenId,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    #[serde(with = "decimal")]
    pub bid_depth: Decimal,
    #[serde(with = "decimal")]
    pub ask_depth: Decimal,
    #[serde(with = "decimal")]
    pub obi: Decimal,
    #[serde(with = "decimal")]
    pub spread: Decimal,
    #[serde(with = "decimal")]
    pub spread_pct: Decimal,
    #[serde(with = "decimal")]
    pub best_bid: Decimal,
    #[serde(with = "decimal")]
    pub best_ask: Decimal,
    #[serde(with = "decimal")]
    pub mid_price: Decimal,
    pub timestamp: i64,
}

impl OrderbookSnapshot {
    /// Build from sorted level vectors, computing depth/obi/spread like the TS
    /// `buildOrderbookSnapshot`.
    pub fn from_levels(
        token_id: impl Into<TokenId>,
        mut bids: Vec<(Decimal, Decimal)>,
        mut asks: Vec<(Decimal, Decimal)>,
        timestamp: i64,
    ) -> Self {
        bids.sort_by(|a, b| b.0.cmp(&a.0));
        asks.sort_by(|a, b| a.0.cmp(&b.0));
        let bid_depth: Decimal = bids.iter().map(|(_, s)| *s).sum();
        let ask_depth: Decimal = asks.iter().map(|(_, s)| *s).sum();
        let total = bid_depth + ask_depth;
        let obi = if total > Decimal::ZERO {
            (bid_depth - ask_depth) / total
        } else {
            Decimal::ZERO
        };
        let best_bid = bids.first().map(|(p, _)| *p).unwrap_or(Decimal::ZERO);
        let best_ask = asks.first().map(|(p, _)| *p).unwrap_or(Decimal::ONE);
        let spread = best_ask - best_bid;
        let mid_price = (best_bid + best_ask) / Decimal::TWO;
        let spread_pct = if mid_price > Decimal::ZERO {
            (spread / mid_price) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        Self {
            token_id: token_id.into(),
            bids,
            asks,
            bid_depth,
            ask_depth,
            obi,
            spread,
            spread_pct,
            best_bid,
            best_ask,
            mid_price,
            timestamp,
        }
    }
}
