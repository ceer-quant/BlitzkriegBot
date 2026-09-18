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
    /// Observation only: the venue's order egress is never constructed.
    ///
    /// This is a THIRD variant rather than a `readonly: bool` flag beside `mode`
    /// for one reason: a flag is opt-in at every site that reads it, and a site
    /// that forgets to read it fails open — it places the real order. A variant
    /// makes every `match` on `Mode` a compile error until it is revisited, so
    /// the guarantee cannot be lost by omission (E12-e / #94).
    ///
    /// Behaviourally read-only settles like [`Mode::Dry`] — because with no venue
    /// there is nothing to report a fill, so a live ledger would be a fiction —
    /// while the *intent* stays visible: `readonly()` answers false for `Dry`
    /// (a simulation, not a promise) and true here.
    ReadOnly,
}

impl Mode {
    /// Whether this mode is a promise never to send an order to a venue.
    ///
    /// Deliberately NOT `mode != Mode::Live`: `Dry` is a simulation, and a
    /// simulation that quietly becomes live is a different thing from an operator
    /// asking for a hard guarantee. Only `ReadOnly` promises.
    pub fn readonly(self) -> bool {
        matches!(self, Mode::ReadOnly)
    }

    /// Whether the venue order executor may be started at all.
    ///
    /// The single predicate every egress site must consult. `Live` is the only
    /// yes: `Dry` has no credentials story and `ReadOnly` exists to refuse.
    pub fn may_trade(self) -> bool {
        matches!(self, Mode::Live)
    }

    /// Whether orders settle as simulated fills rather than waiting on a venue.
    ///
    /// True for both non-trading modes: read-only cannot wait for a venue that
    /// was never started, so it settles the same way a simulation does.
    pub fn settles_locally(self) -> bool {
        !matches!(self, Mode::Live)
    }
}

/// The role an order actually played, RESOLVED FROM ITS FILLS rather than
/// assumed from the requested policy (E17).
///
/// The distinction decides money: a maker fill pays no fee, a taker fill does.
/// Deriving the fee basis from the *request* let a `MakerThenTaker` order be
/// charged as a taker while its trade record claimed maker, and the cash ledger
/// drifted away from `seed + Σ netPnl`.
///
/// State machine, one instance per order, advanced by [`OrderRole::after_fill`]:
///
/// ```text
///                  fill(Maker)                    fill(Taker)
///   Pending ───────────────────────► Maker ─────────────────────► MakerThenTaker
///      │                               ▲                                  │
///      │ fill(Taker)                   │ fill(Maker)                      │ fill(any)
///      ▼                               │                                  ▼
///    Taker ────────────────────────────┘                            MakerThenTaker
/// ```
///
/// `Pending` and `MakerThenTaker` describe an ORDER. A single FILL is always
/// either `Maker` or `Taker` — that is what [`OrderRole::is_maker`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRole {
    /// Submitted, no fill yet: the role is not knowable in advance.
    #[default]
    Pending,
    /// Rested on the book and was hit — post-only, so it never crossed.
    Maker,
    /// Crossed the spread.
    Taker,
    /// Rested first and crossed later: this order's fills are mixed.
    MakerThenTaker,
}

impl OrderRole {
    /// Whether a fill with this role pays no taker fee. Meaningful for a single
    /// fill (`Maker`/`Taker`); `Pending` is treated as "not maker", i.e. the
    /// conservative side that charges a fee.
    pub fn is_maker(self) -> bool {
        matches!(self, Self::Maker)
    }

    /// Fold one fill's role into the order-level role.
    pub fn after_fill(self, fill: OrderRole) -> OrderRole {
        match (self, fill) {
            (Self::Pending, r) => r,
            (Self::Maker, Self::Taker) | (Self::Taker, Self::Maker) => Self::MakerThenTaker,
            (Self::MakerThenTaker, _) => Self::MakerThenTaker,
            (r, _) => r,
        }
    }

    /// The role a fill of an order with this filling behaviour must have had: a
    /// post-only order can only ever be hit as maker, and a `MakerThenTaker`
    /// order rests first — its taker leg is submitted as its own `Taker` order
    /// on escalation — so it fills as maker too. Only a `Taker` order crosses.
    pub fn from_fill_policy(mode: FillPolicy) -> Self {
        match mode {
            FillPolicy::Taker => Self::Taker,
            FillPolicy::Maker | FillPolicy::MakerThenTaker => Self::Maker,
        }
    }
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
    /// The venue's own maker/taker report for this execution (E17-b). When
    /// present it is the AUTHORITY for the fee and for the position's role; when
    /// absent (dry matcher, reconciliation synthesis) the OME falls back to the
    /// order's fill policy. Never inferred from the cumulative size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<bool>,
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
    /// Role this order has actually played, resolved from its fills (E17). Starts
    /// `Pending`; persisted so the audit trail survives a restart.
    #[serde(default)]
    pub role: OrderRole,
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
        bids.sort_by_key(|b| std::cmp::Reverse(b.0));
        asks.sort_by_key(|a| a.0);
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

#[cfg(test)]
mod mode_tests {
    use super::Mode;

    /// The whole point of the third variant: `readonly()` must answer for the
    /// promise, not for "is it dry". A `mode != Live` implementation would pass a
    /// read-only test and also claim a simulation had promised something it never
    /// did — so Dry is asserted false here, not just ReadOnly true.
    #[test]
    fn readonly_is_a_promise_dry_does_not_make() {
        assert!(Mode::ReadOnly.readonly());
        assert!(!Mode::Dry.readonly());
        assert!(!Mode::Live.readonly());
    }

    /// Egress admission. This predicate is what guards construction of the venue
    /// executor, so a regression that widened it would reopen real trading.
    #[test]
    fn only_live_may_reach_a_venue() {
        assert!(Mode::Live.may_trade());
        assert!(!Mode::Dry.may_trade());
        assert!(!Mode::ReadOnly.may_trade());
    }

    /// ReadOnly settles locally: with no venue constructed, nothing would ever
    /// report a fill, so a ledger that waited would simply never move.
    #[test]
    fn everything_but_live_settles_locally() {
        assert!(Mode::Dry.settles_locally());
        assert!(Mode::ReadOnly.settles_locally());
        assert!(!Mode::Live.settles_locally());
    }

    /// The IPC contract carries mode as a lowercase string; a UI gating on
    /// "readonly" needs the exact wire spelling.
    #[test]
    fn wire_spelling_is_stable() {
        let s = |m: Mode| serde_json::to_string(&m).unwrap();
        assert_eq!(s(Mode::Dry), "\"dry\"");
        assert_eq!(s(Mode::Live), "\"live\"");
        assert_eq!(s(Mode::ReadOnly), "\"readonly\"");
    }
}
