//! Shared UI data model — ONE-TO-ONE with the core's IPC messages.
//!
//! These are deliberately a *mirror* of `core/blitzkrieg_core/src/ipc/schema.rs` and
//! `model.rs`, not a re-export: the UI Kit must not link the core's trading
//! crate, so it stays a replaceable presentation layer. Every field name matches
//! the wire (camelCase) so deserialisation is direct.
//!
//! Decimals cross the wire as JSON numbers (the core serialises `Decimal` as an
//! f64) but Polymarket-sourced payloads can use strings, so `de_num` accepts
//! both. The UI only needs display precision, so f64 is appropriate here.

use serde::{Deserialize, Deserializer, Serialize};

/// Accept a JSON number *or* a numeric string → f64.
pub fn de_num<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("bad number")),
        serde_json::Value::String(s) => s.trim().parse::<f64>().map_err(serde::de::Error::custom),
        other => Err(serde::de::Error::custom(format!(
            "expected number/string, got {other}"
        ))),
    }
}

/// Accept number|string|null → Option<f64>.
pub fn de_num_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => Ok(n.as_f64()),
        Some(serde_json::Value::String(s)) => Ok(s.trim().parse::<f64>().ok()),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected number/string, got {other}"
        ))),
    }
}

// ── core.ready ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ReadyView {
    pub version: String,
    pub mode: String,
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub signer: Option<String>,
    #[serde(default)]
    pub funder: Option<String>,
}

// ── ledger.balance ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct BalanceView {
    #[serde(deserialize_with = "de_num")]
    pub balance: f64,
    #[serde(deserialize_with = "de_num")]
    pub reserved: f64,
    #[serde(deserialize_with = "de_num")]
    pub available: f64,
}

// ── positions.list ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionView {
    pub id: String,
    pub asset: String,
    pub direction: String,
    #[serde(default)]
    pub strategy: String,
    pub token_id: String,
    #[serde(deserialize_with = "de_num")]
    pub entry_price: f64,
    #[serde(deserialize_with = "de_num")]
    pub current_price: f64,
    #[serde(deserialize_with = "de_num")]
    pub shares: f64,
    #[serde(deserialize_with = "de_num")]
    pub unrealized_pct: f64,
    #[serde(deserialize_with = "de_num", default)]
    pub high_pnl_pct: f64,
    #[serde(default)]
    pub was_maker_entry: bool,
    #[serde(default)]
    pub entered_at_ms: i64,
    #[serde(default)]
    pub expires_at_ms: i64,
    #[serde(default)]
    pub remaining_sec: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PositionsView {
    pub positions: Vec<PositionView>,
}

// ── engine.round ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPriceView {
    pub asset: String,
    #[serde(deserialize_with = "de_num", default)]
    pub up: f64,
    #[serde(deserialize_with = "de_num", default)]
    pub down: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundView {
    pub slot: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: usize,
    pub can_trade: bool,
    #[serde(default)]
    pub market_prices: Vec<MarketPriceView>,
}

// ── engine.stats ─────────────────────────────────────────────────────────────

/// E9-g: one row of core `engine.stats.strategies[]` — the per-strategy
/// accounting the WebUI plugins/strategies page renders (counters are
/// deserialized defensively: older cores may omit any subset).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyStatsRow {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub orders_placed: u64,
    #[serde(default)]
    pub orders_rejected: u64,
    #[serde(default)]
    pub limit_rejected: u64,
    #[serde(default)]
    pub blocked_timing: u64,
    #[serde(default)]
    pub blocked_momentum: u64,
    #[serde(default)]
    pub gate_exempted_timing: u64,
    #[serde(default)]
    pub gate_exempted_momentum: u64,
    #[serde(default)]
    pub closed_trades: u64,
    #[serde(default)]
    pub wins: u64,
    #[serde(default)]
    pub losses: u64,
    #[serde(default)]
    pub net_pnl_usd: f64,
    #[serde(default)]
    pub rejection_causes: Option<std::collections::BTreeMap<String, u64>>,
    #[serde(default)]
    pub gate_exemptions: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BlockedCounters {
    #[serde(default)]
    pub timing: u64,
    #[serde(default)]
    pub momentum: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatsView {
    #[serde(default)]
    pub books: u64,
    #[serde(default)]
    pub tops: u64,
    #[serde(default)]
    pub spots: u64,
    #[serde(default)]
    pub rounds: u64,
    #[serde(default)]
    pub evaluations: u64,
    #[serde(default)]
    pub signals: u64,
    #[serde(default)]
    pub place_rejected: u64,
    #[serde(default)]
    pub blocked: BlockedCounters,
    #[serde(default)]
    pub confirmed: Vec<String>,
    /// E9-g: per-strategy accounting rows (older cores omit the key entirely).
    #[serde(default)]
    pub strategies: Vec<StrategyStatsRow>,
}

// ── trades.history ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeView {
    pub id: String,
    #[serde(default)]
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    #[serde(deserialize_with = "de_num")]
    pub entry_price: f64,
    #[serde(deserialize_with = "de_num")]
    pub exit_price: f64,
    #[serde(deserialize_with = "de_num")]
    pub shares: f64,
    #[serde(deserialize_with = "de_num")]
    pub net_pnl_usd: f64,
    #[serde(deserialize_with = "de_num", default)]
    pub net_pnl_pct: f64,
    #[serde(default)]
    pub fees_usd: f64,
    #[serde(default)]
    pub exit_reason: String,
    #[serde(default)]
    pub hold_time_sec: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TradesView {
    #[serde(default)]
    pub trades: Vec<TradeView>,
}

// ── strategy.list / extension.list / market.list (plugin manager) ───────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyRow {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyListView {
    pub strategies: Vec<StrategyRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionRow {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionListView {
    pub extensions: Vec<ExtensionRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPluginRow {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub has_data_feed: bool,
    #[serde(default)]
    pub has_discovery: bool,
    #[serde(default)]
    pub has_executor: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketListView {
    /// Core reports the active plugin NAME as a string (absent when none).
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub plugins: Vec<MarketPluginRow>,
}

// ── orders.list ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderView {
    pub order_id: String,
    #[serde(default)]
    pub strategy: String,
    pub asset: String,
    pub direction: String,
    pub token_id: String,
    pub side: String,
    #[serde(default)]
    pub mode: String,
    #[serde(deserialize_with = "de_num")]
    pub price: f64,
    #[serde(deserialize_with = "de_num")]
    pub size: f64,
    #[serde(deserialize_with = "de_num", default)]
    pub filled_size: f64,
    pub status: String,
    #[serde(default)]
    pub round_slot: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrdersView {
    #[serde(default)]
    pub orders: Vec<OrderView>,
}

// ── core.event notifications (kind-tagged) ───────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CoreEvent {
    Ready {
        version: String,
        mode: String,
    },
    OrderUpdate {
        order: OrderView,
    },
    Fill {
        delta: serde_json::Value,
        order: OrderView,
    },
    #[serde(rename_all = "camelCase")]
    PositionClosed {
        id: String,
        asset: String,
        direction: String,
        reason: String,
        #[serde(deserialize_with = "de_num")]
        net_pnl_usd: f64,
        #[serde(deserialize_with = "de_num")]
        net_pnl_pct: f64,
        #[serde(deserialize_with = "de_num")]
        daily_pnl_usd: f64,
    },
    RiskAlert {
        code: serde_json::Value,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    ReconcileReport {
        filled: usize,
        marked_filled: usize,
        marked_cancelled: usize,
        ghost_ids: Vec<String>,
    },
    Error {
        error: serde_json::Value,
    },
    EvolutionSignal {
        signal: serde_json::Value,
    },
    EvolutionApplied {
        signal: serde_json::Value,
    },
    EvolutionRejected {
        signal: serde_json::Value,
        reason: String,
    },
    /// Forward-compatible catch-all so a new core event never breaks the UI.
    #[serde(other)]
    Unknown,
}

// ── Aggregated snapshot the adapters render ─────────────────────────────────

/// A single point-in-time view of the core, assembled by one round of IPC calls.
/// This is what every adapter (web/TUI/app) renders — the adapters differ only
/// in how they present it.
#[derive(Debug, Clone, Default)]
pub struct UiSnapshot {
    pub ready: Option<ReadyView>,
    pub balance: Option<BalanceView>,
    pub round: Option<RoundView>,
    pub stats: Option<EngineStatsView>,
    pub positions: Vec<PositionView>,
    pub orders: Vec<OrderView>,
    pub trades: Vec<TradeView>,
    pub strategies: Vec<StrategyRow>,
    pub extensions: Vec<ExtensionRow>,
    pub market_plugins: Vec<MarketPluginRow>,
    pub market_active: bool,
    /// Name of the active market plugin (identity label for the panel).
    pub market_active_name: Option<String>,
    /// E9-g: per-strategy engine.stats rows (orders/gates/rejection causes).
    pub strategy_stats: Vec<StrategyStatsRow>,
    pub connected: bool,
    pub last_error: Option<String>,
}

impl UiSnapshot {
    /// Realised PnL over the loaded trades (the panel's "net").
    pub fn net_pnl(&self) -> f64 {
        self.trades.iter().map(|t| t.net_pnl_usd).sum()
    }
    pub fn wins(&self) -> usize {
        self.trades.iter().filter(|t| t.net_pnl_usd > 0.0).count()
    }
    pub fn win_rate(&self) -> f64 {
        if self.trades.is_empty() {
            0.0
        } else {
            self.wins() as f64 / self.trades.len() as f64 * 100.0
        }
    }
    pub fn mode(&self) -> &str {
        self.ready
            .as_ref()
            .map(|r| r.mode.as_str())
            .unwrap_or("unknown")
    }
}
