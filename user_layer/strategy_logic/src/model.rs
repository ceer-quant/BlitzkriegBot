//! Core data shapes the strategy logic operates on: orderbook snapshots,
//! directions and token ids. Kept in sync with the kernel's `model` module —
//! the kernel's types are structurally identical, and the engine marshals its
//! snapshots into these before handing them to shared logic.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A token/contract identifier. Newtype-alias so signatures read the same as
/// the kernel's.
pub type TokenId = String;

/// Which side of a binary market a position/signal is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalDirection {
    Up,
    Down,
}

impl SignalDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            SignalDirection::Up => "up",
            SignalDirection::Down => "down",
        }
    }
}

/// A snapshot of the top-of-book plus derived depth/obi metrics. Mirrors the
/// kernel's `OrderbookSnapshot` (and the original TS `buildOrderbookSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderbookSnapshot {
    pub token_id: TokenId,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    #[serde(with = "crate::decimal")]
    pub bid_depth: Decimal,
    #[serde(with = "crate::decimal")]
    pub ask_depth: Decimal,
    #[serde(with = "crate::decimal")]
    pub obi: Decimal,
    #[serde(with = "crate::decimal")]
    pub spread: Decimal,
    #[serde(with = "crate::decimal")]
    pub spread_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub best_bid: Decimal,
    #[serde(with = "crate::decimal")]
    pub best_ask: Decimal,
    #[serde(with = "crate::decimal")]
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
        bids.sort_by_key(|b| std::cmp::Reverse(b.0));
        asks.sort_by_key(|a| a.0);
        Self::from_sorted_levels(token_id, bids, asks, timestamp)
    }

    /// The same construction for input the caller guarantees is already sorted:
    /// `bids` best-first (descending price), `asks` best-first (ascending).
    /// The BTreeMap-backed local book always produces levels in that order, so
    /// its hot per-event snapshot path skips the redundant O(n log n) sort.
    pub fn from_sorted_levels(
        token_id: impl Into<TokenId>,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        timestamp: i64,
    ) -> Self {
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
