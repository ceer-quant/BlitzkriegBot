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

/// Accept the field's own type *or* an explicit JSON `null` → `T::default()`.
///
/// `#[serde(default)]` covers only an ABSENT key. The kernel sends `null` for a
/// block it cannot build — `engine.stats.blocked` is `null` whenever no engine
/// is installed (`--engine` is opt-in) — and a bare `null` into a struct is a
/// hard error that fails the WHOLE payload, so one unavailable counter used to
/// cost the panel every counter (found by the #236 seam test, not by review).
/// Read "null" as "this block is not there", never as a broken snapshot.
pub fn de_null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
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
    /// Starting principal in DRY mode; `None` in LIVE or against an older core.
    #[serde(default, deserialize_with = "de_num_opt")]
    pub seed: Option<f64>,
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

// ── engine.books ─────────────────────────────────────────────────────────────

/// E8-c 盘口深度: one round asset with both token books, as the depth chart
/// renders them. Mirrors the core's `AssetBooksView`; older cores never answer
/// `engine.books`, so a missing reply degrades to an empty vec, not an error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBooksView {
    #[serde(default)]
    pub asset: String,
    #[serde(default)]
    pub up: BookSideView,
    #[serde(default)]
    pub down: BookSideView,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSideView {
    #[serde(default)]
    pub bids: Vec<BookLevelView>,
    #[serde(default)]
    pub asks: Vec<BookLevelView>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub best_bid: Option<f64>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub best_ask: Option<f64>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub mid_price: Option<f64>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub obi: Option<f64>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub spread: Option<f64>,
    #[serde(default, deserialize_with = "de_num_opt")]
    pub spread_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLevelView {
    #[serde(default, deserialize_with = "de_num")]
    pub price: f64,
    #[serde(default, deserialize_with = "de_num")]
    pub size: f64,
}

// ── engine.stats ─────────────────────────────────────────────────────────────

/// E9-g: one row of core `engine.stats.strategies[]` — the per-strategy
/// accounting the WebUI plugins/strategies page renders (counters are
/// deserialized defensively: older cores may omit any subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockedCounters {
    #[serde(default)]
    pub timing: u64,
    #[serde(default)]
    pub momentum: u64,
}

/// #236: the kernel's ONE panel-visible error slot (`engine.stats.lastError`).
/// Structured, not prose: `code` is the same `CoreErrorCode` vocabulary a
/// rejected RPC carries in `data.coreCode`, so a client can branch on it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastErrorView {
    #[serde(default)]
    pub ts_ms: i64,
    /// The `CoreErrorCode` of the failure, as the kernel spells it
    /// (`VENUE_ERROR`, `RISK_REJECTED`, `KILL_SWITCH_ACTIVE`, …).
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

/// The LEGACY spelling of [`LastErrorView`] (`engine.stats.lastVenueError`):
/// same record, message pre-rendered as `<CODE>: <message>`. Read only as the
/// fallback for a core that predates the structured key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyVenueErrorView {
    #[serde(default)]
    pub ts_ms: i64,
    #[serde(default)]
    pub message: String,
}

/// #163: one probe of the trading-capability self-check.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfCheckItemView {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub detail: String,
}

/// The newest self-check report; `null` until one has run (a dry core never
/// runs one, and a restart clears it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfCheckView {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub ts_ms: i64,
    #[serde(default)]
    pub items: Vec<SelfCheckItemView>,
}

/// Kill-switch state. Always present on a core that has the key — the kernel
/// emits `{active:false}` rather than `null` when trading is live — but a core
/// that predates the key leaves this `None`, which must read as "unknown", not
/// as "frozen".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingFrozenView {
    #[serde(default)]
    pub active: bool,
    /// Why trading froze. Only sent while `active` (the kernel omits the key
    /// otherwise), so `None` beside `active == true` means "no reason given".
    #[serde(default)]
    pub reason: Option<String>,
}

/// E31-b: how close the reconcile sweep is to freezing trading. The counter is
/// the missing half of the last error — the error says a sweep failed, the
/// counter says how many in a row, the threshold says when the freeze lands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileView {
    #[serde(default)]
    pub consecutive_sweep_failures: u64,
    #[serde(default)]
    pub freeze_threshold: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Gate tallies. `null` from the kernel when no engine is installed (see
    /// [`de_null_default`]) — an absent engine has no gate, which reads as zero
    /// blocks, not as an unreadable snapshot.
    #[serde(default, deserialize_with = "de_null_default")]
    pub blocked: BlockedCounters,
    #[serde(default)]
    pub confirmed: Vec<String>,
    /// E9-g: per-strategy accounting rows (older cores omit the key entirely).
    #[serde(default)]
    pub strategies: Vec<StrategyStatsRow>,
    /// #226: orders a live venue refused this session (older cores omit).
    #[serde(default)]
    pub venue_rejected: u64,
    /// #180: the kernel's last recorded error — venue refusal, safety net, or
    /// the kernel's own refusal of a leg. The panel's trading-error banner
    /// reads this (structured) key.
    #[serde(default)]
    pub last_error: Option<LastErrorView>,
    /// The legacy spelling of the same record, kept so a panel running against
    /// an older core still has something to show. Never a second, staler copy:
    /// the kernel writes both from one `LastError`.
    #[serde(default)]
    pub last_venue_error: Option<LegacyVenueErrorView>,
    /// E31-b: reconcile-sweep failure streak against the freeze threshold.
    #[serde(default)]
    pub reconcile: Option<ReconcileView>,
    /// #163: newest trading-capability self-check report.
    #[serde(default)]
    pub self_check: Option<SelfCheckView>,
    /// Whether the kill switch is down. The panel's freeze banner reads this.
    #[serde(default)]
    pub trading_frozen: Option<TradingFrozenView>,
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
    /// Milliseconds since epoch (deserialized defensively — older cores omit).
    #[serde(default)]
    pub entry_time: i64,
    #[serde(default)]
    pub exit_time: i64,
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

// ── shadow_evolution.proposals (E13) ────────────────────────────────────────

/// One side of a proposal's comparison block. Decimals cross as strings on the
/// wire, so `de_num` handles both forms.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeMetricsView {
    #[serde(default)]
    pub closed: u32,
    #[serde(default)]
    pub wins: u32,
    #[serde(default, deserialize_with = "de_num")]
    pub win_rate: f64,
    #[serde(default, deserialize_with = "de_num")]
    pub payoff: f64,
    #[serde(default, deserialize_with = "de_num")]
    pub profit_factor: f64,
    #[serde(default, deserialize_with = "de_num")]
    pub net_pnl_usd: f64,
    #[serde(default, deserialize_with = "de_num")]
    pub gross_profit_usd: f64,
    #[serde(default, deserialize_with = "de_num")]
    pub gross_loss_usd: f64,
}

/// One evolvable knob's proposed move: `from → to` (string decimals).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct KnobMove {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

/// One held/historical evolution proposal, one-to-one with the core's
/// `EvolutionProposal` wire form. `from_params`/`to_params` are knob-bags
/// (string decimals), folded into an ordered knob-move list for rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionProposalView {
    pub id: String,
    #[serde(default)]
    pub strategy: String,
    #[serde(default)]
    pub dims: Vec<String>,
    #[serde(default)]
    pub from_params: serde_json::Value,
    #[serde(default)]
    pub to_params: serde_json::Value,
    #[serde(default)]
    pub baseline: TradeMetricsView,
    #[serde(default)]
    pub variant: TradeMetricsView,
    /// Why the evaluator held it ("higher_win_rate" / "better_profit_factor" /
    /// "combined_improvement" — snake_case on the wire).
    #[serde(default)]
    pub reason: String,
    #[serde(default, deserialize_with = "de_num")]
    pub confidence: f64,
    #[serde(default)]
    pub sample_count: u32,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub expires_at_ms: i64,
    /// proposed | deferred | accepted | rejected | expired | superseded
    #[serde(default)]
    pub state: String,
    /// user | auto (`null` while still pending).
    #[serde(default)]
    pub decided_by: Option<String>,
    #[serde(default)]
    pub decided_at_ms: Option<i64>,
    #[serde(default)]
    pub cycle_seq: u64,
}

impl EvolutionProposalView {
    /// Is this proposal still awaiting a decision?
    pub fn is_pending(&self) -> bool {
        self.state == "proposed" || self.state == "deferred"
    }

    /// Ordered knob moves (`[(name, from, to)]`) folded from the raw bags, for
    /// the 对比表's parameter column.
    pub fn knob_moves(&self) -> Vec<(String, String, String)> {
        let (serde_json::Value::Object(from), serde_json::Value::Object(to)) =
            (&self.from_params, &self.to_params)
        else {
            return Vec::new();
        };
        let mut keys: Vec<&String> = from.keys().collect();
        for k in to.keys() {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        keys.sort();
        keys.into_iter()
            .filter_map(|k| {
                let f = from
                    .get(k)
                    .map(|v| v.to_string().trim_matches('"').to_string());
                let t = to
                    .get(k)
                    .map(|v| v.to_string().trim_matches('"').to_string());
                if f == t {
                    return None; // unchanged knobs are not part of the proposal
                }
                Some((
                    k.clone(),
                    f.unwrap_or_else(|| "—".into()),
                    t.unwrap_or_else(|| "—".into()),
                ))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EvolutionProposalsView {
    #[serde(default)]
    pub proposals: Vec<EvolutionProposalView>,
}

/// The shadow-evolution control block the UIs need (from `shadow_evolution.status`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionStatusView {
    #[serde(default)]
    pub auto_evolve: bool,
    #[serde(default)]
    pub last_cycle_ms: i64,
    #[serde(default)]
    pub next_cycle_at_ms: Option<i64>,
    #[serde(default)]
    pub pending_proposals: usize,
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
    /// E13: a variant was held as a proposal — the operator decides.
    EvolutionProposed {
        proposal: serde_json::Value,
    },
    /// E13: the 72h deep round fired.
    #[serde(rename_all = "camelCase")]
    EvolutionCycle {
        cycle_seq: u64,
        dims: usize,
        strategies: Vec<String>,
        at_ms: i64,
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
    /// All-time closed-trade totals (trades.summary; older cores omit).
    pub trade_summary: Option<serde_json::Value>,
    /// Name of the active market plugin (identity label for the panel).
    pub market_active_name: Option<String>,
    /// E9-g: per-strategy engine.stats rows (orders/gates/rejection causes).
    pub strategy_stats: Vec<StrategyStatsRow>,
    /// E8-c 盘口深度: per-asset L2 depth (older cores omit → empty).
    pub books: Vec<AssetBooksView>,
    /// E13: every known proposal's latest state, newest first (older cores
    /// refuse the method → empty).
    pub evolution_proposals: Vec<EvolutionProposalView>,
    /// E13: the shadow-evolution control block (`None` = older core).
    pub evolution_status: Option<EvolutionStatusView>,
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
