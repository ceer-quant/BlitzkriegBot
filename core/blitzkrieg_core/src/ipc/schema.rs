//! IPC contract — Unix-domain-socket JSON-RPC 2.0.
//!
//! These serde types are the SINGLE schema between Rust core and Node. Node
//! mirrors them with zod; there is no `any`/`unknown` on the wire.
//!
//! Framing: one JSON object per line (`\n`), UTF-8.
//!
//!   Node → Rust  request:      { jsonrpc:"2.0", id, method, params }
//!   Rust → Node  response:     { jsonrpc:"2.0", id, result | error }
//!   Rust → Node  notification: { jsonrpc:"2.0", method:"core.event", params: Event }
//!
//! Node never sends a private key; the core loads credentials itself.

use crate::model::*;
use crate::ome::FillDelta;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub const JSONRPC: &str = "2.0";
pub const EVENT_METHOD: &str = "core.event";

// ── Envelope ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "rpc_version")]
    pub jsonrpc: String,
    /// Protocol/schema version (standardisation: every IPC message carries one).
    #[serde(default = "protocol_version")]
    pub version: String,
    /// Any JSON value (string/number/null); echoed verbatim.
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Current protocol version reported/expected on the IPC boundary.
pub const PROTOCOL_VERSION: &str = "1.1";
fn protocol_version() -> String {
    PROTOCOL_VERSION.into()
}
fn rpc_version() -> String {
    JSONRPC.into()
}

#[derive(Debug, Clone, Serialize)]
pub struct Success {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ErrorData>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorData {
    pub core_code: CoreErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub error: RpcError,
}

impl Success {
    pub fn new(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            result,
        }
    }
}
impl Failure {
    pub fn new(id: serde_json::Value, code: i32, message: String, data: Option<ErrorData>) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            error: RpcError {
                code,
                message,
                data,
            },
        }
    }
    /// JSON-RPC reserved codes.
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    /// Application error (see CoreErrorCode in data.core_code).
    pub const APPLICATION: i32 = -32000;
}

// ── Method names ─────────────────────────────────────────────────────────────

pub mod method {
    pub const PING: &str = "core.ping";
    pub const READY: &str = "core.ready";
    pub const RISK_KILL: &str = "risk.kill";
    pub const RISK_RESUME: &str = "risk.resume";
    pub const ORDER_PLACE: &str = "orders.place";
    pub const ORDER_CANCEL: &str = "orders.cancel";
    pub const ORDER_CANCEL_ALL: &str = "orders.cancel_all";
    pub const ORDER_LIST: &str = "orders.list";
    pub const LEDGER_BALANCE: &str = "ledger.balance";
    /// List open positions (enriched with current price / unrealised PnL).
    pub const POSITIONS_LIST: &str = "positions.list";
    /// Force-close a position at market (manual flatten).
    pub const POSITION_EXIT: &str = "positions.exit";
    /// Closed-trade history (Node-compatible records) for the UI.
    pub const TRADES_HISTORY: &str = "trades.history";
    pub const TRADES_SUMMARY: &str = "trades.summary";
    /// Manually trigger a reconciliation sweep with a caller-supplied snapshot
    /// (live venue snapshot is gathered internally in live mode; this method is
    /// mainly for tests/ops).
    pub const ORDER_RECONCILE: &str = "orders.reconcile";
    /// P0 bridge: Node pushes the latest L2 snapshot so DRY mode can simulate
    /// fills. Replaced in P3 when market-data ingestion moves into the core.
    pub const BOOK_SNAPSHOT: &str = "books.snapshot";
    /// Top-of-book only update (price_change / best_bid_ask).
    pub const TOP_OF_BOOK: &str = "books.top";
    /// Data-feed bridge: deliver a full L2 book straight to the self-driving
    /// engine (`Core::engine_on_data`) — the same choke point the Rust-native
    /// market feed (`--feed-ws`) drives and the one the backtester replays.
    /// Unlike `books.snapshot` it does NOT run the DRY maker-fill simulation, so
    /// a session captured through this method replays decision-for-decision.
    pub const ENGINE_BOOK: &str = "engine.book";
    /// Binance spot price tick (feeds the momentum filter).
    pub const SPOT_PRICE: &str = "spot.price";
    /// Current round state (slot / timing / market count).
    pub const ENGINE_ROUND: &str = "engine.round";
    /// Read-only depth view: the round's assets with each side's live L2 levels,
    /// best prices, OBI and spread — what the WebUI's 盘口深度 chart renders.
    /// Same books the engine trades on; purely observational, no side effects.
    pub const ENGINE_BOOKS: &str = "engine.books";
    /// Diagnostic snapshot: feed counters + confirmed trend tokens.
    pub const ENGINE_STATS: &str = "engine.stats";
    /// List strategies registered in the kernel's strategy engine.
    pub const STRATEGY_LIST: &str = "strategy.list";
    /// Enable/disable a strategy by name.
    pub const STRATEGY_ENABLE: &str = "strategy.enable";
    /// Load a user-layer strategy shared library (feature `strategy-loading`).
    pub const STRATEGY_LOAD: &str = "strategy.load";
    /// Unload a dynamic strategy (drop instance + free the library). Refuses
    /// in-tree or still-enabled strategies and open positions (E9-b).
    pub const STRATEGY_UNLOAD: &str = "strategy.unload";
    /// Atomic swap (E9-b): load the NEW dylib under the SAME registered name,
    /// dropping the old instance only after the new one parses; enable state
    /// is preserved. `params: {name, path}`.
    pub const STRATEGY_RELOAD: &str = "strategy.reload";
    /// Extension lifecycle: list / enable / disable / uninstall.
    pub const EXTENSION_LIST: &str = "extension.list";
    pub const EXTENSION_ENABLE: &str = "extension.enable";
    pub const EXTENSION_DISABLE: &str = "extension.disable";
    /// Market plugins: list registered market extensions (name/type/capabilities).
    pub const MARKET_LIST: &str = "market.list";
    /// Shadow Evolution control surface (opt-in feature). Every mutation is
    /// per-strategy (E2-c): `apply`/`rollback` name ONE strategy, and `status`
    /// reports each evolved strategy's own block.
    pub const SE_ENABLE: &str = "shadow_evolution.enable";
    pub const SE_DISABLE: &str = "shadow_evolution.disable";
    pub const SE_STATUS: &str = "shadow_evolution.status";
    pub const SE_HISTORY: &str = "shadow_evolution.history";
    pub const SE_ROLLBACK: &str = "shadow_evolution.rollback";
    pub const SE_APPLY: &str = "shadow_evolution.apply";
    /// Supply the current round's UP/DOWN markets (discovered by Node's scanner
    /// in P3-transition; the Rust scanner takes over in P4).
    pub const ENGINE_MARKETS: &str = "engine.markets";
}

// ── Typed params / results ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceParams {
    #[serde(flatten)]
    pub order: OrderRequest,
    /// Resting maker order escalates to taker after this many ms if unfilled
    /// (order modes maker_then_taker). 0 = rest until cancelled/expired.
    #[serde(default)]
    pub maker_timeout_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaceResult {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CancelParams {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
}
#[derive(Debug, Clone, Serialize)]
pub struct CancelResult {
    pub success: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAllParams {
    #[serde(default)]
    pub token_id: Option<TokenId>,
}
#[derive(Debug, Clone, Serialize)]
pub struct CancelAllResult {
    pub cancelled: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceResult {
    #[serde(with = "crate::decimal")]
    pub balance: Decimal,
    #[serde(with = "crate::decimal")]
    pub reserved: Decimal,
    #[serde(with = "crate::decimal")]
    pub available: Decimal,
    /// Starting principal in DRY mode (`--seed-balance`), so a UI can show the
    /// `principal + realized net − reserved` reconciliation rather than merely
    /// asserting it. `None` in LIVE, where the principal is whatever the venue
    /// reported at start and is not a number the core fixed itself. Absent on
    /// older cores, which the reader must treat as "unknown", not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyResult {
    pub version: String,
    pub mode: Mode,
    pub authenticated: bool,
    pub signer: Option<String>,
    pub funder: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookLevel {
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    #[serde(with = "crate::decimal")]
    pub size: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSnapshotParams {
    pub token_id: TokenId,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    #[serde(default)]
    pub ts_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileParams {
    /// Exchange order ids currently resting.
    #[serde(default)]
    pub open_order_ids: Vec<String>,
    #[serde(default)]
    pub trades: Vec<ReconcileTrade>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileTrade {
    #[serde(rename = "venueOrderId")]
    pub venue_order_id: String,
    #[serde(rename = "tradeId")]
    pub trade_id: String,
    #[serde(rename = "tokenId")]
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "crate::decimal")]
    pub size: Decimal,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    #[serde(default)]
    pub ts_ms: i64,
    /// The venue's own maker/taker report for this trade, when Node knows it.
    /// Absent means "fall back to the order's fill policy".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResultView {
    pub filled: usize,
    pub marked_filled: usize,
    pub marked_cancelled: usize,
    pub ghost_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopOfBookParams {
    pub token_id: TokenId,
    #[serde(default, with = "crate::decimal::opt")]
    pub best_bid: Option<Decimal>,
    #[serde(default, with = "crate::decimal::opt")]
    pub best_ask: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotPriceParams {
    pub asset: String,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineMarketsParams {
    pub markets: Vec<CryptoMarket>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionView {
    pub id: String,
    pub asset: String,
    pub direction: String,
    pub strategy: String,
    pub token_id: TokenId,
    #[serde(with = "crate::decimal")]
    pub entry_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub current_price: Decimal,
    #[serde(with = "crate::decimal")]
    pub shares: Decimal,
    #[serde(with = "crate::decimal")]
    pub unrealized_pct: Decimal,
    #[serde(with = "crate::decimal")]
    pub high_pnl_pct: Decimal,
    pub was_maker_entry: bool,
    pub entered_at_ms: i64,
    pub expires_at_ms: i64,
    pub remaining_sec: i64,
    /// The ACTUAL cash flows accrued on this position so far (E17). Exposed so an
    /// external monitor can verify the accounting identity at any instant, not
    /// only once the position is closed:
    ///
    /// ```text
    ///   balance == seed + Σ_closed net
    ///                    − Σ_open (entry_cost_usd + entry_fee_usd)
    ///                    + Σ_open (proceeds_usd − exit_fee_usd)
    /// ```
    ///
    /// Note the identity uses `entry_cost_usd` (TOTAL paid on the way in), NOT
    /// `cost_usd`: `cost_usd` is the basis of the shares STILL HELD, so a partial
    /// exit releases part of it and would otherwise be double-counted against the
    /// `proceeds_usd` that already returned it.
    ///
    /// A partially-exited position has both sides non-zero, which is exactly the
    /// case a flat-only check cannot see.
    #[serde(with = "crate::decimal")]
    pub entry_cost_usd: Decimal,
    /// Basis of the shares STILL HELD — a position-book figure, not a cash one.
    /// It shrinks on every partial exit (the released basis comes back inside
    /// `proceeds_usd`), so it must not be summed into the cash identity above.
    #[serde(with = "crate::decimal")]
    pub cost_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub entry_fee_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub proceeds_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub exit_fee_usd: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionExitParams {
    /// "all" flattens every open position; otherwise a specific position id.
    #[serde(default)]
    pub position_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionExitResult {
    pub closed: usize,
}

// ── Server → Node events ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
    Ready {
        version: String,
        mode: Mode,
    },
    OrderUpdate {
        order: TrackedOrder,
    },
    /// Canonical position effect derived from an authoritative fill.
    Fill {
        delta: FillDeltaView,
        order: TrackedOrder,
    },
    /// A position was closed (PnL realised); Node renders the human log.
    PositionClosed {
        id: String,
        asset: String,
        direction: String,
        reason: String,
        #[serde(with = "crate::decimal", rename = "netPnlUsd")]
        net_pnl_usd: Decimal,
        #[serde(with = "crate::decimal", rename = "netPnlPct")]
        net_pnl_pct: Decimal,
        #[serde(with = "crate::decimal", rename = "dailyPnlUsd")]
        daily_pnl_usd: Decimal,
    },
    RiskAlert {
        code: CoreErrorCode,
        message: String,
    },
    /// Outcome of a reconciliation sweep (also emitted when nothing changed only
    /// if there were ghosts/actions; quiet sweeps emit nothing).
    ReconcileReport {
        filled: usize,
        #[serde(rename = "markedFilled")]
        marked_filled: usize,
        #[serde(rename = "markedCancelled")]
        marked_cancelled: usize,
        #[serde(rename = "ghostIds")]
        ghost_ids: Vec<String>,
    },
    Error {
        error: CoreError,
    },
    /// Shadow Evolution: a proposal was produced (pre-application).
    EvolutionSignal {
        signal: crate::shadow_evolution::EvolveSignal,
    },
    /// Shadow Evolution: parameters were atomically swapped in.
    EvolutionApplied {
        signal: crate::shadow_evolution::EvolveSignal,
    },
    /// Shadow Evolution: a proposal was rejected by a safety lock (or a rollback).
    EvolutionRejected {
        signal: crate::shadow_evolution::EvolveSignal,
        reason: String,
    },
}

/// Wire view of FillDelta (field names match Node conventions).
#[derive(Debug, Clone, Serialize)]
pub struct FillDeltaView {
    #[serde(rename = "orderId")]
    pub order_id: OrderId,
    #[serde(rename = "tokenId")]
    pub token_id: TokenId,
    pub side: Side,
    #[serde(with = "crate::decimal")]
    pub delta: Decimal,
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    #[serde(with = "crate::decimal")]
    pub cumulative: Decimal,
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    #[serde(rename = "conditionId")]
    pub condition_id: String,
}

impl From<FillDelta> for FillDeltaView {
    fn from(d: FillDelta) -> Self {
        Self {
            order_id: d.order_id,
            token_id: d.token_id,
            side: d.side,
            delta: d.delta,
            price: d.price,
            cumulative: d.cumulative,
            strategy: d.strategy,
            asset: d.asset,
            direction: d.direction,
            condition_id: d.condition_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Event,
}
impl Notification {
    pub fn new(event: Event) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            method: EVENT_METHOD.into(),
            params: event,
        }
    }
}
