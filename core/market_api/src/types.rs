//! Market plugin domain types.
//!
//! These are the market-agnostic primitives and boundary DTOs shared by the
//! trading core and every market extension. They were lifted out of the core's
//! `model.rs` / `order/mod.rs` verbatim (same serde attributes, so the wire
//! format is unchanged) plus the new plugin-boundary DTOs.
//!
//! Nothing here performs I/O, and nothing here knows any specific venue.

use crate::decimal;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub type TokenId = String;
pub type OrderId = String;
pub type TradeId = String;

/// Which side of a binary market a position/signal is on. (Also the generic
/// order side; the name is retained for wire compatibility.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Flip the side. Venue trades report the taker side; a maker fill on the
    /// opposite side is what reduces our resting order.
    pub fn invert(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    Gtc,
    Fok,
    Fak,
    Gtd,
}

/// How an order should fill, mirroring the Node strategy `OrderMode`. This is
/// what the simulator and venue branch on; GTC/FOK/post-only are derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillPolicy {
    /// Cross immediately (FOK-style), fill at the buffered limit.
    Taker,
    /// Rest a post-only bid/offer; fill only when the book crosses.
    Maker,
    /// Rest as maker, then cancel and cross as taker after maker_timeout_ms.
    MakerThenTaker,
}

impl FillPolicy {
    pub fn venue_order_type(self) -> OrderType {
        match self {
            FillPolicy::Taker => OrderType::Fok,
            FillPolicy::Maker | FillPolicy::MakerThenTaker => OrderType::Gtc,
        }
    }
    pub fn post_only(self) -> bool {
        matches!(self, FillPolicy::Maker | FillPolicy::MakerThenTaker)
    }
    pub fn immediate(self) -> bool {
        matches!(self, FillPolicy::Taker)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    /// Accepted into the core but not yet acknowledged by the venue/sim.
    Pending,
    /// Resting on the book (or fully live in the sim).
    Live,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    /// Submitted then reported failed by the venue; provisional effects rolled back.
    Failed,
}

impl OrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Rejected | Self::Failed
        )
    }
    pub fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Live | Self::PartiallyFilled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FillStatus {
    Matched,
    Mined,
    Confirmed,
    Failed,
}

impl FillStatus {
    /// Larger = later/more authoritative, mirroring the Node priority table.
    pub fn priority(self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Matched => 1,
            Self::Mined => 2,
            Self::Confirmed => 3,
        }
    }
}

/// Structured error code crossing the plugin boundary and IPC. `raw` always
/// carries the venue message so Node never has to flatten or swallow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CoreErrorCode {
    InvalidParams,
    UnknownOrder,
    WouldCross,
    InvalidTickSize,
    InvalidSize,
    InsufficientFunds,
    RiskRejected,
    KillSwitchActive,
    MarketHalted,
    NotAuthenticated,
    VenueError,
    Timeout,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreError {
    pub code: CoreErrorCode,
    pub message: String,
    /// Raw venue/SDK error text, never altered or hidden from Node.
    pub raw: Option<String>,
}

impl CoreError {
    pub fn new(code: CoreErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            raw: None,
        }
    }
    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.raw = Some(raw.into());
        self
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for CoreError {}

pub type CoreResult<T> = Result<T, CoreError>;

// ── Order intent (market-agnostic) ───────────────────────────────────────────

/// Which class of market an intent targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketType {
    Prediction,
    Spot,
    Futures,
    Options,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderKind {
    Limit,
    Market,
    StopLimit,
    PostOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TimeInForce {
    Gtc,
    Fok,
    Fak,
    Gtd,
}

impl From<TimeInForce> for OrderType {
    fn from(t: TimeInForce) -> Self {
        match t {
            TimeInForce::Gtc => OrderType::Gtc,
            TimeInForce::Fok => OrderType::Fok,
            TimeInForce::Fak => OrderType::Fak,
            TimeInForce::Gtd => OrderType::Gtd,
        }
    }
}

/// A market-agnostic order request. Field names mirror the IPC `OrderRequest`
/// where they overlap so adapters can map without lossy conversions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderIntent {
    pub market: MarketType,
    pub symbol: String,
    pub side: Side,
    pub order_kind: OrderKind,
    #[serde(with = "crate::decimal::opt", default)]
    pub price: Option<Decimal>,
    #[serde(with = "crate::decimal")]
    pub size: Decimal,
    pub time_in_force: TimeInForce,
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, String>,
}

impl OrderIntent {
    /// Confirm the intent is internally consistent for submission.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.symbol.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "symbol is required",
            ));
        }
        if self.size <= Decimal::ZERO {
            return Err(CoreError::new(
                CoreErrorCode::InvalidSize,
                "size must be positive",
            ));
        }
        let needs_price = matches!(
            self.order_kind,
            OrderKind::Limit | OrderKind::StopLimit | OrderKind::PostOnly
        );
        if needs_price && self.price.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "limit/stop/post-only orders require a price",
            ));
        }
        if let Some(p) = self.price
            && (p <= Decimal::ZERO || p > Decimal::ONE)
            && self.market == MarketType::Prediction
        {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "prediction price must be in (0,1]",
            ));
        }
        Ok(())
    }
}

// ── Market descriptor ────────────────────────────────────────────────────────

/// A binary UP/DOWN market for one asset in the current round.
///
/// Stage 1 keeps the historical field shape verbatim so the wire format and all
/// existing call sites are untouched; venue-specific keys are folded into
/// `metadata` in Stage 2 (when discovery/scanner move into the extension).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketDescriptor {
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

// ── Plugin boundary DTOs ─────────────────────────────────────────────────────

/// Full L2 snapshot for one token (top-of-book plus depth). Raw levels, not a
/// derived `OrderbookSnapshot`, so the core rebuilds its own local book.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookUpdate {
    pub token_id: TokenId,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    pub ts_ms: i64,
}

/// Top-of-book-only update (price_change / best_bid_ask carry no depth).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopOfBookUpdate {
    pub token_id: TokenId,
    #[serde(with = "decimal::opt", default)]
    pub best_bid: Option<Decimal>,
    #[serde(with = "decimal::opt", default)]
    pub best_ask: Option<Decimal>,
    pub ts_ms: i64,
}

/// Spot reference price for one asset (feeds the momentum filter).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotUpdate {
    pub asset: String,
    #[serde(with = "decimal")]
    pub price: Decimal,
    pub ts_ms: i64,
}

/// An order the core has accepted locally but not yet placed on a venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingOrder {
    pub core_order_id: String,
    pub token_id: TokenId,
    pub condition_id: String,
    pub side: Side,
    pub fill_policy: FillPolicy,
    #[serde(with = "decimal")]
    pub price: Decimal,
    #[serde(with = "decimal")]
    pub size: Decimal,
    pub internal_key: String,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub round_slot: i64,
}

/// An authoritative execution reported by a venue (per-execution; the OME
/// dedups by size cap).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketFill {
    pub order_id: String,
    pub trade_id: Option<String>,
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "decimal")]
    pub price: Decimal,
    #[serde(with = "decimal")]
    pub size: Decimal,
    pub status: FillStatus,
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
    /// The venue's OWN report of which side of the book this execution landed
    /// on: `Some(true)` when we rested and were hit (maker), `Some(false)` when
    /// we crossed (taker). A venue knows this exactly — Polymarket reports the
    /// trade's `taker_order_id` separately from its `maker_orders[]` — so it must
    /// not be re-derived downstream from the policy we *asked* for. `None` when
    /// the source cannot say (dry matcher, reconciliation), which lets the core
    /// fall back to the order's own fill policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<bool>,
}

/// A single venue trade used for reconciliation (per-trade size, not cumulative).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VenueTradeInfo {
    pub venue_order_id: String,
    pub trade_id: String,
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "decimal")]
    pub size: Decimal,
    #[serde(with = "decimal")]
    pub price: Decimal,
    pub ts_ms: i64,
    pub tx_hash: Option<String>,
    /// Whether this trade rested on our side of the book (maker) rather than
    /// crossing (taker). Same authority as `MarketFill::maker`; a gap fill
    /// synthesised from it must charge the fee the venue actually did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<bool>,
}

/// Authoritative venue snapshot for the periodic reconciliation sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileSnapshot {
    pub open_order_ids: Vec<String>,
    pub trades: Vec<VenueTradeInfo>,
    pub now_ms: i64,
}

// ── Plugin configuration ─────────────────────────────────────────────────────

/// Data-feed parameters (orderbook subscription + optional spot stream).
#[derive(Debug, Clone, Default)]
pub struct DataFeedConfig {
    /// Spot reference symbols the feed should stream (e.g. ["BTC","ETH"]).
    pub spot_assets: Vec<String>,
    /// Override the orderbook websocket URL (None = venue default).
    pub ws_url: Option<String>,
}

/// Round-discovery parameters.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub assets: Vec<String>,
    pub round_duration_sec: i64,
    pub poll_sec: u64,
}

/// Order-execution parameters.
#[derive(Debug, Clone, Default)]
pub struct ExecutorConfig {
    /// Condition ids to subscribe on the live user channel (empty = REST only).
    pub markets: Vec<String>,
}

// ── Plugin introspection (for `market.list`) ─────────────────────────────────

/// Static capability report for one registered market plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub name: String,
    pub market_type: MarketType,
    pub has_data_feed: bool,
    pub has_discovery: bool,
    pub has_executor: bool,
    /// True once at least one of the plugin's components is running.
    pub enabled: bool,
    /// True for the single plugin actually driving this process.
    pub active: bool,
}
