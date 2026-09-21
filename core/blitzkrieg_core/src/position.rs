//! Position manager — open positions, PnL, exit checks, cooldowns, capacity.
//!
//! Rust port of `src/strategies/crypto-hft/positions.ts` (deleted with the Node
//! source layer, `62b16c88`). Decision logic is delegated to the pure
//! `exit_policy` module; this module owns the stateful parts:
//! the open/closed books, daily PnL, and per-asset/direction cooldowns.

use crate::exit_policy::{
    ExitConfig, ExitState, ExitTickInput, decide_exit_verdict, effective_stop_pct, executable_bid,
    pnl_pct, reference_price, update_exit_state,
};
use crate::model::{ExitReason, OrderRole, OrderbookSnapshot, Side, SignalDirection};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Venue share-size grid on the binary markets: 1 tick = 0.01 shares. An exit is
/// floored to it so a SELL can never oversell the position.
fn share_grid() -> Decimal {
    Decimal::new(1, 2)
}

/// Largest grid multiple that is <= `v`.
pub(crate) fn floor_to_grid(v: Decimal) -> Decimal {
    let grid = share_grid();
    (v / grid).floor() * grid
}

/// A fee rate over a notional, as a percentage (0 when there is no notional).
fn pct_of(fee: Decimal, notional: Decimal) -> Decimal {
    if notional > Decimal::ZERO {
        (fee / notional) * Decimal::ONE_HUNDRED
    } else {
        Decimal::ZERO
    }
}

/// The ACTUAL cash flows accrued on one position over its life.
///
/// This is the accounting spine of E17: `gross = proceeds − entry_cost` and
/// `net = gross − entry_fee − exit_fee` are computed from amounts the ledger
/// really charged, never re-derived from a fee rate and a price. That makes
/// `balance == seed + Σ net_pnl_usd` an identity rather than a coincidence, so
/// it cannot drift on partial fills, mixed maker/taker roles or unsellable dust.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CashFlows {
    /// Σ notional paid on the entry fills.
    pub entry_cost_usd: Decimal,
    /// Σ cash fee the ledger charged on the entry fills.
    pub entry_fee_usd: Decimal,
    /// Σ notional received on the exit fills.
    pub proceeds_usd: Decimal,
    /// Σ cash fee the ledger charged on the exit fills.
    pub exit_fee_usd: Decimal,
    /// Σ shares bought (the closed trade's size).
    pub opened_shares: Decimal,
    /// Σ shares sold across all exit fills.
    pub sold_shares: Decimal,
}

#[derive(Debug, Clone)]
pub struct PositionConfig {
    pub exit: ExitConfig,
    pub max_positions: usize,
    /// Absolute daily-loss cap in USD. `0` = no absolute cap: the relative cap
    /// below is then the whole budget. Kept because an operator may want a hard
    /// dollar number regardless of account size.
    pub max_daily_loss_usd: Decimal,
    /// Daily-loss cap as a percentage of the day's OPENING cash equity. `0` =
    /// off. This is the default budget precisely because an absolute default is
    /// meaningless across account sizes: the old hard-coded 200 USD was an
    /// unbounded budget for a 4.8 USDC book (P0 #173).
    pub max_daily_loss_equity_pct: Decimal,
    /// Where the per-day realized-loss budget is persisted (a JSON sibling of
    /// the position log, like `positions.recon`). `None` = memory only — the
    /// kernel wires it from `--position-log`'s path; tests leave it unset so
    /// separate managers never share a file.
    pub daily_pnl_path: Option<String>,
    /// Minimum separation between two "stop suppressed by the wick guard"
    /// alerts for the SAME position (P0 #177): a persistent wick must be
    /// visible in review without flooding the panel once per tick. `0` = every
    /// tick reports.
    pub stop_suppression_repeat_sec: i64,
    pub stop_loss_cooldown_sec: i64,
    pub exit_cooldown_sec: i64,
    pub asset_cooldown_sec: i64,
    pub loss_cooldown_sec: i64,
}

impl Default for PositionConfig {
    fn default() -> Self {
        Self {
            exit: ExitConfig::default(),
            max_positions: 2,
            // Off by default; the relative budget below carries the protection.
            max_daily_loss_usd: Decimal::ZERO,
            // 20% of the day's opening cash equity. Meaningful on any account
            // size, unlike a fixed dollar default.
            max_daily_loss_equity_pct: Decimal::from(20),
            daily_pnl_path: None,
            stop_suppression_repeat_sec: 30,
            stop_loss_cooldown_sec: 180,
            exit_cooldown_sec: 60,
            asset_cooldown_sec: 90,
            loss_cooldown_sec: 180,
        }
    }
}

/// Milliseconds in one UTC day.
const DAY_MS: i64 = 86_400_000;

/// The UTC day index (days since the Unix epoch) a timestamp falls in.
///
/// The venue declares no trading day (`round_slot` is a 900s round, not a
/// session), so the day boundary is the kernel's own. UTC is chosen over a
/// local-midnight or rolling 24h window because the kernel's OTHER daily
/// boundary — the event-archive segment name — is already UTC
/// (`data_source::utc_stamp`), and a fixed-offset day has no DST ambiguity to
/// reason about when a loss is attributed to "today".
pub fn utc_day_index(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// The per-day realized-loss budget, persisted so a restart cannot launder
/// today's losses (P0 #173).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DailyLossState {
    /// UTC day this budget belongs to; `None` = no day opened yet in this
    /// process (the first tick stamps it).
    pub day_index: Option<i64>,
    /// Realized PnL accrued since the day opened (USD, negative = loss).
    pub realized_pnl_usd: Decimal,
    /// Cash equity when the day opened — the base the percentage cap is
    /// measured against. Fixed for the day on purpose: a shrinking equity must
    /// not shrink the budget it is being measured against.
    pub opening_equity_usd: Decimal,
    /// True once the cap was breached; entries stay frozen until the next day.
    pub tripped: bool,
    pub tripped_at_ms: i64,
    /// The cap that tripped (USD), for the panel/alert after a restart.
    pub tripped_limit_usd: Decimal,
    /// Whether the trip has already been announced. Deliberately NOT persisted:
    /// after a restart the operator must again be told that entries are frozen.
    #[serde(skip)]
    pub tripped_reported: bool,
    pub opened_at_ms: i64,
}

impl Default for DailyLossState {
    fn default() -> Self {
        Self {
            day_index: None,
            realized_pnl_usd: Decimal::ZERO,
            opening_equity_usd: Decimal::ZERO,
            tripped: false,
            tripped_at_ms: 0,
            tripped_limit_usd: Decimal::ZERO,
            tripped_reported: false,
            opened_at_ms: 0,
        }
    }
}

/// A day boundary was crossed: the budget starts fresh.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyRoll {
    pub day_index: i64,
    pub previous_day_index: Option<i64>,
    /// The closed day's realized PnL (0 when no day was open).
    pub previous_realized_pnl_usd: Decimal,
    /// Cash equity the new day opens with.
    pub opening_equity_usd: Decimal,
    /// The effective cap for the new day (USD); 0 = no cap configured.
    pub limit_usd: Decimal,
    /// True when the PREVIOUS day had tripped (worth reporting at the roll).
    pub previous_tripped: bool,
}

/// The daily-loss breaker tripped: new entries are frozen for the rest of the
/// UTC day. Reported once per process (see `DailyLossState::tripped_reported`).
#[derive(Debug, Clone, PartialEq)]
pub struct DailyTrip {
    pub day_index: i64,
    pub realized_pnl_usd: Decimal,
    pub limit_usd: Decimal,
    pub opening_equity_usd: Decimal,
    pub at_ms: i64,
}

/// Why a protective stop that wanted to fire did not become an exit order.
///
/// Both causes mean the same thing to the operator — "I should have stopped out
/// and did not" — but they need different fixes, so they are told apart rather
/// than flattened into one alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSuppressionCause {
    /// The wick guard: the raw bid breached the stop but the mid did not
    /// confirm, so the breach looked like a wick rather than a move (#177).
    WickGuard,
    /// F6: the exit rule fired, but no bid was both live and fresh enough to
    /// price a SELL against. Sending an order priced off a number no buyer was
    /// showing is the thing the F6 gate forbids — so the position is held, and
    /// the held exit is reported here instead of vanishing.
    NoExecutableQuote,
}

/// One exit the F6 gate withheld from becoming an order — "should have
/// triggered" made visible to review and to the panel (P0 #177, F6).
#[derive(Debug, Clone, PartialEq)]
pub struct SuppressedStopEvent {
    pub cause: StopSuppressionCause,
    pub position_id: String,
    pub token_id: String,
    pub strategy: String,
    pub asset: String,
    pub entry_price: Decimal,
    /// The bid that was rejected as a price: zero when the book showed no bid
    /// at all, the stale level when it showed one past the staleness budget.
    /// For [`StopSuppressionCause::WickGuard`] it is the raw bid that breached
    /// the stop.
    pub bid: Decimal,
    /// For [`StopSuppressionCause::WickGuard`]: the mid that failed to confirm.
    /// For [`StopSuppressionCause::NoExecutableQuote`]: the last known price the
    /// stop actually judged on, since no bid was available to report.
    pub mid: Decimal,
    pub pnl_pct_at_bid: Decimal,
    pub pnl_pct_at_mid: Decimal,
    pub stop_pct: Decimal,
    pub now_ms: i64,
}

impl SuppressedStopEvent {
    /// The audit-trail line: what the raw bid would have done, what the mid
    /// said instead, and the stop that was therefore not taken.
    pub fn message(&self) -> String {
        match self.cause {
            StopSuppressionCause::WickGuard => format!(
                "stop SUPPRESSED (wick guard): {} {} entry {} bid {} (-{}%) mid {} ({}%) stop {}% — \
                 raw bid breached the stop, mid did not confirm",
                self.position_id,
                self.asset,
                self.entry_price,
                self.bid,
                self.pnl_pct_at_bid.abs(),
                self.mid,
                self.pnl_pct_at_mid,
                self.stop_pct
            ),
            StopSuppressionCause::NoExecutableQuote if self.bid > Decimal::ZERO => format!(
                "stop SUPPRESSED (stale quote): {} {} entry {} bid {} ({}%) stop {}% — the exit \
                 rule fired, but the book was past its staleness budget, so that bid could not \
                 price a SELL; the position is held until a fresh quote appears or expiry settles it",
                self.position_id,
                self.asset,
                self.entry_price,
                self.bid,
                self.pnl_pct_at_bid,
                self.stop_pct
            ),
            StopSuppressionCause::NoExecutableQuote => format!(
                "stop SUPPRESSED (no executable bid): {} {} entry {} last {} ({}%) stop {}% — the \
                 exit rule fired, but the book showed no bid at all, so no order was sent; the \
                 position is held until a buyer appears or expiry settles it",
                self.position_id,
                self.asset,
                self.entry_price,
                self.mid,
                self.pnl_pct_at_mid,
                self.stop_pct
            ),
        }
    }
}

/// The durable side file holding the day's loss budget. Same convention as the
/// position log's `.recon` watermark: one small file, best-effort writes, a
/// missing/corrupt file never blocks startup (memory stays authoritative).
struct DailyLossStore {
    path: std::path::PathBuf,
}

impl DailyLossStore {
    fn new(path: impl Into<String>) -> Self {
        Self {
            path: std::path::PathBuf::from(path.into()),
        }
    }

    fn save(&self, st: &DailyLossState) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(st) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&self.path, text) {
                    tracing::warn!(error = %e, path = %self.path.display(), "daily-loss persist failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "daily-loss serialize failed"),
        }
    }

    fn load(&self) -> Option<DailyLossState> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        match serde_json::from_str::<DailyLossState>(&text) {
            Ok(st) => Some(st),
            Err(e) => {
                tracing::warn!(error = %e, path = %self.path.display(), "daily-loss state unreadable — starting a fresh day");
                None
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPosition {
    pub id: String,
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    pub entry_price: Decimal,
    pub current_price: Decimal,
    pub prev_price: Decimal,
    /// Shares still held.
    pub shares: Decimal,
    /// Cost basis still held (entry notionals of the shares above).
    pub cost_usd: Decimal,
    /// True when no taker fee was paid on the way in.
    pub was_maker_entry: bool,
    /// Entry fee as a percentage of the entry basis — a VIEW of
    /// [`CashFlows::entry_fee_usd`], kept for the trade-record/panel contract.
    pub entry_fee_pct: Decimal,
    /// Role the ENTRY fills actually played (E17 audit trail). Mixed maker/taker
    /// entries resolve to `MakerThenTaker`.
    #[serde(default)]
    pub entry_role: OrderRole,
    /// Role the EXIT fills have played so far.
    #[serde(default)]
    pub exit_role: OrderRole,
    pub target_exit_price: Option<Decimal>,
    pub entered_at_ms: i64,
    pub expires_at_ms: i64,
    /// F6: when the exit path last received a book for this token (ms epoch).
    /// A forced exit may consult it to refuse pricing off a stale quote; it is
    /// `0` until the first book arrives (serde default keeps old snapshots
    /// loading as "no book seen since restart").
    #[serde(default)]
    pub last_book_ts: i64,
    pub state: ExitState,
    /// Actual cash flows since the position opened. Snapshots written before E17
    /// carry the serde default (all zero) and are repaired by [`OpenPosition::flows`].
    #[serde(default)]
    pub flows: CashFlows,
}

impl OpenPosition {
    pub fn high_pnl_pct(&self) -> Decimal {
        self.state.high_pnl_pct
    }
    pub fn low_pnl_pct(&self) -> Decimal {
        self.state.low_pnl_pct
    }

    /// The position's cash flows, repaired for a snapshot persisted before E17
    /// recorded them: the pre-E17 fields (`cost_usd`, `entry_fee_pct`) describe
    /// the same money, so they seed the totals and the next save converges.
    pub fn flows(&self) -> CashFlows {
        if self.flows.opened_shares > Decimal::ZERO || self.flows.entry_cost_usd > Decimal::ZERO {
            return self.flows.clone();
        }
        CashFlows {
            entry_cost_usd: self.cost_usd,
            entry_fee_usd: (self.entry_fee_pct / Decimal::ONE_HUNDRED)
                * self.entry_price
                * self.shares,
            opened_shares: self.shares,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClosedPosition {
    pub id: String,
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    /// Weighted-average entry price (= basis / shares) — unchanged semantics.
    pub entry_price: Decimal,
    /// Volume-weighted average price of the exit fills. Before E17 this was the
    /// price of the single closing fill; on a partial ladder it is now the mean.
    pub exit_price: Decimal,
    /// Shares opened (what the fee/PnL maths is denominated in).
    pub shares: Decimal,
    /// Entry basis in USD, straight from the ledger (not re-derived).
    pub cost_usd: Decimal,
    pub was_maker_entry: bool,
    pub was_maker_exit: bool,
    pub entry_fee_pct: Decimal,
    pub exit_fee_pct: Decimal,
    /// Gross PnL in USD: proceeds − basis. Σ over closed trades equals the
    /// ledger's realized cash movement by construction.
    pub pnl_usd: Decimal,
    pub pnl_pct: Decimal,
    pub net_pnl_usd: Decimal,
    pub net_pnl_pct: Decimal,
    pub high_pnl_pct: Decimal,
    pub low_pnl_pct: Decimal,
    pub hold_time_sec: i64,
    pub exit_reason: ExitReason,
    pub entered_at_ms: i64,
    pub exited_at_ms: i64,
    /// Roles the two legs actually played (E17 audit trail).
    pub entry_role: OrderRole,
    pub exit_role: OrderRole,
    /// Unsold shares written off at close (below the 0.01 share grid, or beyond
    /// the held size). Non-zero only in that corner; see [`PositionManager::close`].
    pub dust_shares: Decimal,
}

pub struct OpenParams {
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    /// The order's limit price; the position's actual entry price is the basis
    /// per share once its fills land (see [`PositionManager::apply_entry_fill`]).
    pub entry_price: Decimal,
    pub expires_at_ms: i64,
    /// Role the entry order was REQUESTED as; informational at open time (the
    /// fee comes from the fill's own role in [`PositionManager::apply_entry_fill`]).
    pub was_maker: bool,
    pub target_exit_price: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct ExitRequest {
    pub position_id: String,
    pub reason: ExitReason,
    pub exit_price: Decimal,
    pub use_maker: bool,
}

pub struct PositionManager {
    config: PositionConfig,
    open: Vec<OpenPosition>,
    closed: Vec<ClosedPosition>,
    next_id: u64,
    /// The day's realized-loss budget (P0 #173). `daily_pnl` is now a VIEW of
    /// this state rather than a separate field, so persistence cannot drift.
    daily: DailyLossState,
    daily_store: Option<DailyLossStore>,
    last_stop_loss_at: i64,
    exit_cooldowns: std::collections::HashMap<String, i64>,
    asset_last_exit_at: std::collections::HashMap<String, i64>,
    asset_last_loss_at: std::collections::HashMap<String, i64>,
    /// Withheld protective stops waiting for the kernel to report them (P0 #177).
    suppressed_stops: Vec<SuppressedStopEvent>,
    /// Total suppressed stops reported over this process's life (panel counter).
    suppressed_stop_count: u64,
    /// Last time a suppression was reported, per position — the throttle.
    last_suppression_at: std::collections::HashMap<String, i64>,
}

fn cooldown_key(asset: &str, direction: SignalDirection) -> String {
    format!("{asset}_{}", direction.as_str())
}

impl PositionManager {
    pub fn new(config: PositionConfig) -> Self {
        let daily_store = config.daily_pnl_path.clone().map(DailyLossStore::new);
        // A persisted budget must gate entries BEFORE the first tick, so it is
        // loaded here rather than lazily: a restart on a day whose cap is
        // already spent must not accept one entry first.
        let daily = daily_store
            .as_ref()
            .and_then(|s| s.load())
            .unwrap_or_default();
        Self {
            config,
            open: Vec::new(),
            closed: Vec::new(),
            next_id: 1,
            daily,
            daily_store,
            last_stop_loss_at: 0,
            exit_cooldowns: Default::default(),
            asset_last_exit_at: Default::default(),
            asset_last_loss_at: Default::default(),
            suppressed_stops: Vec::new(),
            suppressed_stop_count: 0,
            last_suppression_at: Default::default(),
        }
    }

    pub fn set_config(&mut self, config: PositionConfig) {
        let path_changed = config.daily_pnl_path != self.config.daily_pnl_path;
        self.config = config;
        if path_changed {
            self.daily_store = self.config.daily_pnl_path.clone().map(DailyLossStore::new);
            // A different budget file is a different budget: re-seed from it,
            // keeping the in-memory state when the new file has none.
            if let Some(loaded) = self.daily_store.as_ref().and_then(|s| s.load()) {
                self.daily = loaded;
            }
        }
    }

    pub fn open_positions(&self) -> &[OpenPosition] {
        &self.open
    }

    /// Rebuild the open book from durable storage (crash recovery). Replaces any
    /// in-memory state and advances `next_id` past the restored ids so a new
    /// position can never collide with a recovered one (`hft-N`).
    pub fn restore_open(&mut self, positions: Vec<OpenPosition>) {
        let max_id = positions
            .iter()
            .filter_map(|p| p.id.strip_prefix("hft-"))
            .filter_map(|n| n.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        if max_id + 1 > self.next_id {
            self.next_id = max_id + 1;
        }
        self.open = positions;
    }
    pub fn closed_positions(&self) -> &[ClosedPosition] {
        &self.closed
    }
    pub fn daily_pnl(&self) -> Decimal {
        self.daily.realized_pnl_usd
    }

    /// The day's realized-loss budget as the panel/CLI report it.
    pub fn daily_state(&self) -> &DailyLossState {
        &self.daily
    }

    /// The effective daily-loss cap in USD: the TIGHTER of the absolute cap and
    /// the percentage of the day's opening cash equity. `0` = no cap (both off,
    /// or the percentage configured but the account size still unknown).
    pub fn effective_daily_loss_limit(&self) -> Decimal {
        let abs = self.config.max_daily_loss_usd;
        let pct = self.config.max_daily_loss_equity_pct;
        let rel = if pct > Decimal::ZERO && self.daily.opening_equity_usd > Decimal::ZERO {
            self.daily.opening_equity_usd * pct / Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        match (abs > Decimal::ZERO, rel > Decimal::ZERO) {
            (true, true) => abs.min(rel),
            (true, false) => abs,
            (false, true) => rel,
            (false, false) => Decimal::ZERO,
        }
    }

    /// True when the day's realized loss reached the effective cap. Sticky for
    /// the rest of the day so a later winning trade cannot re-open entries.
    pub fn daily_loss_tripped(&self) -> bool {
        if self.daily.tripped {
            return true;
        }
        let limit = self.effective_daily_loss_limit();
        limit > Decimal::ZERO && self.daily.realized_pnl_usd <= -limit
    }

    /// Roll the budget over when `now_ms` crosses into a new UTC day, seeding
    /// the new day's opening equity from `equity_usd` (the caller's cash
    /// equity: the position manager cannot read the ledger itself).
    ///
    /// Returns `Some` exactly when a new day opened, so the caller can put the
    /// old day's result in the audit trail. The FIRST call only stamps the day
    /// (nothing to close yet) and reports it, so the budget's start is visible.
    pub fn roll_daily(&mut self, now_ms: i64, equity_usd: Decimal) -> Option<DailyRoll> {
        let today = utc_day_index(now_ms);
        if self.daily.day_index == Some(today) {
            // Same day: only keep the opening equity meaningful when the day
            // was stamped without one (a restored budget from before the
            // account size was known).
            if self.daily.opening_equity_usd <= Decimal::ZERO && equity_usd > Decimal::ZERO {
                self.daily.opening_equity_usd = equity_usd;
                self.persist_daily();
            }
            return None;
        }
        let previous_day_index = self.daily.day_index;
        let previous_realized = self.daily.realized_pnl_usd;
        let previous_tripped = self.daily.tripped;
        let carried_pnl = if previous_day_index.is_none() {
            // First stamp of the day (fresh process, or before this field
            // existed): it only starts the clock. Whatever was realized before
            // the first tick belongs to TODAY, so it is carried, never zeroed.
            previous_realized
        } else {
            // A real boundary: the closed day's result is reported, then the new
            // day starts from zero.
            Decimal::ZERO
        };
        let carried_trip = previous_day_index.is_none() && previous_tripped;
        self.daily = DailyLossState {
            day_index: Some(today),
            realized_pnl_usd: carried_pnl,
            opening_equity_usd: equity_usd.max(Decimal::ZERO),
            tripped: carried_trip,
            tripped_at_ms: if carried_trip {
                self.daily.tripped_at_ms
            } else {
                0
            },
            tripped_limit_usd: if carried_trip {
                self.daily.tripped_limit_usd
            } else {
                Decimal::ZERO
            },
            tripped_reported: false,
            opened_at_ms: now_ms,
        };
        self.last_stop_loss_at = 0;
        self.exit_cooldowns.clear();
        self.persist_daily();
        Some(DailyRoll {
            day_index: today,
            previous_day_index,
            previous_realized_pnl_usd: previous_realized,
            opening_equity_usd: self.daily.opening_equity_usd,
            limit_usd: self.effective_daily_loss_limit(),
            previous_tripped,
        })
    }

    /// The day's realized PnL, reset. Kept for callers that roll the budget
    /// themselves; [`roll_daily`] is the kernel's path because it also seeds
    /// the opening equity and persists.
    pub fn reset_daily(&mut self) {
        self.daily.realized_pnl_usd = Decimal::ZERO;
        self.daily.tripped = false;
        self.daily.tripped_at_ms = 0;
        self.daily.tripped_limit_usd = Decimal::ZERO;
        self.daily.tripped_reported = false;
        self.last_stop_loss_at = 0;
        self.exit_cooldowns.clear();
        self.persist_daily();
    }

    /// The trip to report, consumed once per process (see
    /// `DailyLossState::tripped_reported`). `None` while the day is healthy.
    pub fn take_daily_trip(&mut self) -> Option<DailyTrip> {
        if !self.daily_loss_tripped() {
            return None;
        }
        if !self.daily.tripped {
            // Detected here rather than only in `close` (a restored budget can
            // arrive already breached): latch it so the panel agrees.
            self.daily.tripped = true;
            self.daily.tripped_at_ms = self.daily.opened_at_ms;
            self.daily.tripped_limit_usd = self.effective_daily_loss_limit();
        }
        if self.daily.tripped_reported {
            return None;
        }
        self.daily.tripped_reported = true;
        self.persist_daily();
        Some(DailyTrip {
            day_index: self.daily.day_index.unwrap_or(0),
            realized_pnl_usd: self.daily.realized_pnl_usd,
            limit_usd: self.daily.tripped_limit_usd,
            opening_equity_usd: self.daily.opening_equity_usd,
            at_ms: self.daily.tripped_at_ms,
        })
    }

    fn persist_daily(&self) {
        if let Some(store) = self.daily_store.as_ref() {
            store.save(&self.daily);
        }
    }

    /// Latch the breaker if the day's realized loss just reached the cap. Called
    /// from [`close`](Self::close) so the freeze and its persistence happen with
    /// the loss, not one tick later.
    fn note_daily_drawdown(&mut self, now_ms: i64) {
        let limit = self.effective_daily_loss_limit();
        if limit > Decimal::ZERO && self.daily.realized_pnl_usd <= -limit && !self.daily.tripped {
            self.daily.tripped = true;
            self.daily.tripped_at_ms = now_ms;
            self.daily.tripped_limit_usd = limit;
            tracing::warn!(
                day = self.daily.day_index.unwrap_or(0),
                realized = %self.daily.realized_pnl_usd,
                limit = %limit,
                equity = %self.daily.opening_equity_usd,
                "daily loss limit reached — new entries frozen until the next UTC day"
            );
        }
        self.persist_daily();
    }

    pub fn open(&mut self, p: OpenParams, now_ms: i64) -> OpenPosition {
        let id = format!("hft-{}", self.next_id);
        self.next_id += 1;
        // The requested role only seeds the view; the fee and the role are fixed
        // by the actual entry fill(s) in `apply_entry_fill` (E17-b).
        let entry_role = if p.was_maker {
            OrderRole::Maker
        } else {
            OrderRole::Taker
        };
        let pos = OpenPosition {
            id,
            strategy: p.strategy,
            asset: p.asset,
            direction: p.direction,
            token_id: p.token_id,
            condition_id: p.condition_id,
            entry_price: p.entry_price,
            current_price: p.entry_price,
            prev_price: p.entry_price,
            shares: Decimal::ZERO,
            cost_usd: Decimal::ZERO,
            was_maker_entry: p.was_maker,
            entry_fee_pct: Decimal::ZERO,
            entry_role,
            exit_role: OrderRole::Pending,
            target_exit_price: p.target_exit_price,
            entered_at_ms: now_ms,
            expires_at_ms: p.expires_at_ms,
            last_book_ts: 0,
            state: ExitState::new(p.entry_price, now_ms),
            flows: CashFlows::default(),
        };
        self.open.push(pos.clone());
        pos
    }

    /// Accrue one ENTRY fill: basis, fee, shares and the resolved role — all from
    /// the amounts the ledger actually moved. Returns the new position snapshot.
    ///
    /// `shares`/`fee_usd` may be NEGATIVE (a `FAILED` rollback of an earlier
    /// fill): the accrual then reverses by the same amounts, so the position ends
    /// where it started instead of keeping phantom shares the cash ledger has
    /// already refunded.
    pub fn apply_entry_fill(
        &mut self,
        position_id: &str,
        shares: Decimal,
        price: Decimal,
        fee_usd: Decimal,
        role: OrderRole,
    ) -> Option<OpenPosition> {
        let pos = self.open.iter_mut().find(|p| p.id == position_id)?;
        let signed_cost = price * shares;
        pos.flows.opened_shares = (pos.flows.opened_shares + shares).max(Decimal::ZERO);
        pos.flows.entry_cost_usd = (pos.flows.entry_cost_usd + signed_cost).max(Decimal::ZERO);
        pos.flows.entry_fee_usd = (pos.flows.entry_fee_usd + fee_usd).max(Decimal::ZERO);
        // Basis and share count move together, so `cost_usd / shares` is always
        // the average paid for the shares still held.
        pos.shares = (pos.shares + shares).max(Decimal::ZERO);
        pos.cost_usd = (pos.cost_usd + signed_cost).max(Decimal::ZERO);
        if pos.shares == Decimal::ZERO {
            pos.cost_usd = Decimal::ZERO;
        }
        if shares > Decimal::ZERO {
            // A reversal is not a fill, so it neither picks a role nor re-anchors
            // the exit state.
            pos.entry_role = pos.entry_role.after_fill(role);
        }
        Self::refresh_view(pos);
        // Averaging into an existing position moves the basis, so the exit state
        // must follow — otherwise HWM/trails stay anchored to a stale entry.
        if shares > Decimal::ZERO {
            pos.state = ExitState::new(pos.entry_price, pos.entered_at_ms);
        }
        Some(pos.clone())
    }

    /// Accrue one EXIT fill (a partial close). Returns the position's new share
    /// count, or None if the position is unknown. A negative `shares` reverses an
    /// earlier exit fill (rolled back by the venue) and returns its basis.
    pub fn apply_exit_fill(
        &mut self,
        position_id: &str,
        shares: Decimal,
        price: Decimal,
        fee_usd: Decimal,
        role: OrderRole,
    ) -> Option<Decimal> {
        let pos = self.open.iter_mut().find(|p| p.id == position_id)?;
        pos.flows.sold_shares = (pos.flows.sold_shares + shares).max(Decimal::ZERO);
        pos.flows.proceeds_usd = (pos.flows.proceeds_usd + price * shares).max(Decimal::ZERO);
        pos.flows.exit_fee_usd = (pos.flows.exit_fee_usd + fee_usd).max(Decimal::ZERO);
        if shares > Decimal::ZERO {
            pos.exit_role = pos.exit_role.after_fill(role);
        }
        // Basis leaves at the position's own average, so what remains stays
        // proportional to the shares still held (no drift on a partial ladder).
        // A reversal adds it back at that same average.
        let per_share = if pos.shares > Decimal::ZERO {
            pos.cost_usd / pos.shares
        } else {
            Decimal::ZERO
        };
        let released = (per_share * shares).min(pos.cost_usd);
        pos.shares = (pos.shares - shares).max(Decimal::ZERO);
        pos.cost_usd = (pos.cost_usd - released).max(Decimal::ZERO);
        if pos.shares == Decimal::ZERO {
            pos.cost_usd = Decimal::ZERO;
        }
        Some(pos.shares)
    }

    /// Drop an open position that never really existed (a fully rolled-back
    /// entry leaves zero shares and zero basis; it must not block `can_open` or
    /// show up as an empty row on the panel). Returns true when one was dropped.
    pub fn drop_if_empty(&mut self, position_id: &str) -> bool {
        let idx = self.open.iter().position(|p| {
            p.id == position_id && p.shares <= Decimal::ZERO && p.cost_usd == Decimal::ZERO
        });
        match idx {
            Some(i) => {
                self.open.remove(i);
                true
            }
            None => false,
        }
    }

    /// Fold the accrued flows back into the display/audit fields of an open
    /// position: the fee rate is a VIEW of the fee actually charged, and the
    /// entry price is the basis per share still held.
    fn refresh_view(pos: &mut OpenPosition) {
        pos.entry_fee_pct = pct_of(pos.flows.entry_fee_usd, pos.flows.entry_cost_usd);
        pos.was_maker_entry = pos.flows.entry_fee_usd == Decimal::ZERO;
        if pos.shares > Decimal::ZERO {
            pos.entry_price = pos.cost_usd / pos.shares;
        }
    }

    /// Update a position's exit state from a fresh book.
    pub fn tick(&mut self, position_id: &str, book: Option<&OrderbookSnapshot>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            Self::valuate_one(pos, book, now_ms, &cfg);
        }
    }

    /// Update a single position's exit state AND its `current_price` from a book.
    /// This is what makes the UI's unrealized PnL move; it must run independently
    /// of whether automated exits are enabled.
    ///
    /// F6: `current_price` is a REFERENCE valuation (bid, else two-sided mid,
    /// else the stale value) — the dashboard may keep showing it. It is never
    /// again a substitute for an executable quote: exit decisions and SELL fills
    /// price off `executable_bid`, which is zero when no bid quotes.
    fn valuate_one(
        pos: &mut OpenPosition,
        book: Option<&OrderbookSnapshot>,
        now_ms: i64,
        cfg: &ExitConfig,
    ) {
        update_exit_state(&mut pos.state, pos.entry_price, book, now_ms, cfg);
        let val = reference_price(book, pos.current_price);
        if val > Decimal::ZERO && pos.current_price != val {
            pos.prev_price = pos.current_price;
            pos.current_price = val;
        }
        // Track the freshness of the last book the valuation actually saw: the
        // SNAPSHOT's own receive time when the caller supplies one (backtest /
        // replay), else `now`. The exit path uses this to refuse pricing a SELL
        // off a stale quote. NOTE: the live service rebuilds cached snapshots
        // with `timestamp = now_ms`, which always looks fresh — preserving the
        // real receive time through that cache is a service-layer follow-up.
        if let Some(b) = book {
            pos.last_book_ts = if b.timestamp > 0 { b.timestamp } else { now_ms };
        }
    }

    /// Re-value every open position from the latest books (no exit decisions).
    /// Safe to call even when automated exits are disabled, so the dashboard
    /// always shows live unrealized PnL and HWM.
    pub fn valuate(&mut self, books: &dyn Fn(&str) -> Option<OrderbookSnapshot>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        for pos in self.open.iter_mut() {
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
        }
    }

    /// Evaluate every open position; returns exit requests for those that hit a
    /// rule. Updates exit state from the book first so the decision and the
    /// recorded HWM share the same quote.
    pub fn check_exits(
        &mut self,
        books: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
        now_ms: i64,
    ) -> Vec<ExitRequest> {
        let cfg = self.config.exit.clone();
        let mut out = Vec::new();
        let mut withheld: Vec<SuppressedStopEvent> = Vec::new();
        for pos in self.open.iter_mut() {
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
            // F6: the exit price is an EXECUTABLE bid or nothing. The old code
            // fell back to `pos.current_price` — a stale reference that could
            // be an arbitrary number of seconds old, or a one-sided-book
            // phantom — and let forced exits book profit no buyer was offering.
            // A book older than the staleness budget is not a quote either: a
            // bid captured minutes ago is not a buyer standing here now.
            //
            // This gate prices the ORDER, and only the order. It must not skip
            // the DECISION: `decide_exit_verdict` already draws the same line
            // (a mandatory or profit-side exit needs a live bid; the protective
            // stop may read a fresh last-known price), and short-circuiting
            // here made a breached stop indistinguishable from a quiet market —
            // no order AND no report. So the verdict runs either way, and a
            // decision that cannot be priced becomes a held-stop report below.
            let exit_price = executable_bid(book.as_ref());
            let book_fresh = match book.as_ref() {
                Some(b) => b.timestamp <= 0 || now_ms - b.timestamp <= cfg.max_book_age_sec * 1000,
                None => true,
            };
            let priceable = exit_price > Decimal::ZERO && book_fresh;
            let time_left_sec = (pos.expires_at_ms - now_ms) / 1000;
            let hold_sec = (now_ms - pos.entered_at_ms) / 1000;

            // Fixed-target strategies (e.g. sharp_reversal).
            if let Some(t) = pos.target_exit_price
                && priceable
                && exit_price >= t
                && hold_sec >= self.config.exit.exit_grace_sec
            {
                out.push(ExitRequest {
                    position_id: pos.id.clone(),
                    reason: ExitReason::TakeProfit,
                    exit_price: t,
                    use_maker: true,
                });
                continue;
            }

            // The verdict carries back a protective stop the wick guard
            // withheld, so "should have triggered" is reportable rather than
            // silent (P0 #177).
            let verdict = decide_exit_verdict(ExitTickInput {
                entry_price: pos.entry_price,
                book: book.as_ref(),
                fallback_price: Some(pos.current_price),
                time_left_sec,
                hold_sec,
                state: &pos.state,
                now_ms,
                cfg: &cfg,
            });
            if let Some(s) = verdict.suppressed_stop {
                withheld.push(SuppressedStopEvent {
                    cause: StopSuppressionCause::WickGuard,
                    position_id: pos.id.clone(),
                    token_id: pos.token_id.clone(),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    entry_price: pos.entry_price,
                    bid: s.bid,
                    mid: s.mid,
                    pnl_pct_at_bid: s.pnl_pct_at_bid,
                    pnl_pct_at_mid: s.pnl_pct_at_mid,
                    stop_pct: s.stop_pct,
                    now_ms,
                });
            }
            if let Some(d) = verdict.decision {
                if priceable {
                    out.push(ExitRequest {
                        position_id: pos.id.clone(),
                        reason: d.reason,
                        exit_price,
                        use_maker: d.use_maker,
                    });
                } else {
                    // The rule fired but there is no buyer to sell to at any
                    // price we can name — hold, and report the held exit so it
                    // reaches review instead of dying in memory.
                    withheld.push(SuppressedStopEvent {
                        cause: StopSuppressionCause::NoExecutableQuote,
                        position_id: pos.id.clone(),
                        token_id: pos.token_id.clone(),
                        strategy: pos.strategy.clone(),
                        asset: pos.asset.clone(),
                        entry_price: pos.entry_price,
                        bid: exit_price,
                        mid: pos.current_price,
                        pnl_pct_at_bid: if exit_price > Decimal::ZERO {
                            pnl_pct(exit_price, pos.entry_price)
                        } else {
                            Decimal::ZERO
                        },
                        pnl_pct_at_mid: pnl_pct(pos.current_price, pos.entry_price),
                        stop_pct: effective_stop_pct(cfg.stop_loss_pct, time_left_sec, &cfg),
                        now_ms,
                    });
                }
            }
        }
        for ev in withheld {
            self.note_suppressed_stop(ev);
        }
        out
    }

    /// Record a withheld protective stop, throttled per position so a wick that
    /// persists for minutes does not become a per-tick alert storm. The first
    /// occurrence is always recorded.
    fn note_suppressed_stop(&mut self, ev: SuppressedStopEvent) {
        let gap_ms = self.config.stop_suppression_repeat_sec.max(0) * 1000;
        if let Some(&last) = self.last_suppression_at.get(&ev.position_id)
            && gap_ms > 0
            && ev.now_ms - last < gap_ms
        {
            return;
        }
        self.last_suppression_at
            .insert(ev.position_id.clone(), ev.now_ms);
        self.suppressed_stop_count += 1;
        self.suppressed_stops.push(ev);
    }

    /// Take the withheld-stop reports accumulated since the last drain. The
    /// kernel turns each one into a `RiskAlert`, so the suppression reaches the
    /// panel and the audit trail instead of dying in memory.
    pub fn drain_suppressed_stops(&mut self) -> Vec<SuppressedStopEvent> {
        std::mem::take(&mut self.suppressed_stops)
    }

    /// How many suppressed stops have been reported in this process (panel counter).
    pub fn suppressed_stop_count(&self) -> u64 {
        self.suppressed_stop_count
    }
    /// Close a position, computing gross/net PnL and setting cooldowns.
    ///
    /// Everything here is derived from the ACCRUED cash flows (E17-d), so
    /// `net_pnl_usd` is exactly the cash the ledger moved for this position:
    ///
    /// ```text
    ///   gross = proceeds        − entry_cost
    ///   net   = gross − entry_fee − exit_fee
    /// ```
    ///
    /// An unsold remainder that cannot be placed on the venue's 0.01 share grid
    /// is written off at the exit price and charged to `net` as a rounding cost.
    /// That keeps the invariant true in the corner case instead of leaking a
    /// little cash every cycle, and it is visible on the record as `dust_shares`
    /// rather than hidden in a fudge.
    pub fn close(
        &mut self,
        position_id: &str,
        exit_price: Decimal,
        reason: ExitReason,
        was_maker: bool,
        now_ms: i64,
    ) -> Option<ClosedPosition> {
        let idx = self.open.iter().position(|p| p.id == position_id)?;
        let mut pos = self.open.remove(idx);
        pos.flows = pos.flows();

        // The caller's flag is the role of the ORDER it just placed; fold it in so
        // the record reflects every exit fill, including earlier partials.
        pos.exit_role = pos.exit_role.after_fill(if was_maker {
            OrderRole::Maker
        } else {
            OrderRole::Taker
        });

        let opened = pos.flows.opened_shares;
        let sold = pos.flows.sold_shares.min(opened);
        let dust = (opened - sold).max(Decimal::ZERO);
        let dust_notional = exit_price * dust;
        let exit_notional = pos.flows.proceeds_usd + dust_notional;

        // Shares with no exit fill of their own (a direct close, or the sub-grid
        // remainder) are priced here, so they must carry a fee here too — at the
        // role of the order the caller just placed. F8: the fee follows the
        // configured fee model, not a hard-wired curve.
        let dust_fee = if was_maker {
            Decimal::ZERO
        } else {
            (self.config.exit.fee_model.fee_pct(exit_price) / Decimal::ONE_HUNDRED) * dust_notional
        };

        let cost = pos.flows.entry_cost_usd;
        let entry_fee_usd = pos.flows.entry_fee_usd;
        let exit_fee_usd = pos.flows.exit_fee_usd + dust_fee;
        let gross = exit_notional - cost;
        let net = gross - entry_fee_usd - exit_fee_usd;
        let net_pct = if cost > Decimal::ZERO {
            (net / cost) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        };
        let avg_exit = if sold > Decimal::ZERO {
            pos.flows.proceeds_usd / sold
        } else {
            exit_price
        };
        let exit_fee_pct = pct_of(exit_fee_usd, exit_notional);
        let hold_time_sec = (now_ms - pos.entered_at_ms) / 1000;

        self.daily.realized_pnl_usd += net;
        if reason == ExitReason::StopLoss {
            self.last_stop_loss_at = now_ms;
        }
        self.exit_cooldowns
            .insert(cooldown_key(&pos.asset, pos.direction), now_ms);
        self.asset_last_exit_at.insert(pos.asset.clone(), now_ms);
        if net < Decimal::ZERO {
            self.asset_last_loss_at.insert(pos.asset.clone(), now_ms);
        }
        // The freeze and its durable record travel WITH the loss (P0 #173).
        self.note_daily_drawdown(now_ms);
        self.last_suppression_at.remove(&pos.id);

        let closed = ClosedPosition {
            id: pos.id,
            strategy: pos.strategy,
            asset: pos.asset,
            direction: pos.direction,
            token_id: pos.token_id,
            condition_id: pos.condition_id,
            entry_price: pos.entry_price,
            exit_price: avg_exit,
            shares: opened,
            cost_usd: cost,
            was_maker_entry: entry_fee_usd == Decimal::ZERO,
            was_maker_exit: exit_fee_usd == Decimal::ZERO,
            entry_fee_pct: pct_of(entry_fee_usd, cost),
            exit_fee_pct,
            pnl_usd: gross,
            pnl_pct: pnl_pct(avg_exit, pos.entry_price),
            net_pnl_usd: net,
            net_pnl_pct: net_pct,
            high_pnl_pct: pos.state.high_pnl_pct,
            low_pnl_pct: pos.state.low_pnl_pct,
            hold_time_sec,
            exit_reason: reason,
            entered_at_ms: pos.entered_at_ms,
            exited_at_ms: now_ms,
            entry_role: pos.entry_role,
            exit_role: pos.exit_role,
            dust_shares: dust,
        };
        self.closed.push(closed.clone());
        if self.closed.len() > 5000 {
            let excess = self.closed.len() - 5000;
            self.closed.drain(0..excess);
        }
        Some(closed)
    }

    /// Sell size for a full exit: exactly the shares held, floored to the venue's
    /// 0.01 share grid so the order can never oversell (E17-d).
    ///
    /// Before E17 this subtracted a flat 0.01 buffer, which left that buffer
    /// unsold on an exact-grid position — permanent dust, and cash that could
    /// never be reconciled. Flooring to the grid gives the same oversell
    /// protection with no remainder: a position of 10 sells 10, not 9.99.
    pub fn sell_shares(&self, position_id: &str) -> Option<Decimal> {
        let pos = self.open.iter().find(|p| p.id == position_id)?;
        let grid = floor_to_grid(pos.shares);
        if grid <= Decimal::ZERO {
            return None;
        }
        Some(grid)
    }

    /// Adjust an open position's entry price and share count for an external
    /// reconciliation that did not arrive as a fill (no-op if unknown).
    ///
    /// Deliberately does NOT touch [`CashFlows`]: an adjustment is not cash, and
    /// letting it rewrite the fee basis is exactly the class of divergence E17
    /// closed. Real fills go through `apply_entry_fill` / `apply_exit_fill`.
    pub fn adjust_open(&mut self, position_id: &str, entry_price: Decimal, shares: Decimal) {
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            pos.entry_price = entry_price;
            pos.shares = shares;
            pos.cost_usd = entry_price * shares;
        }
    }

    /// F4: undo one `close()`. The venue reported the closing fill FAILED, so
    /// the position was never sold: put the pre-close row back (shares, basis,
    /// accrued flows, exit state — exactly as it stood), hand back the daily
    /// PnL the close added, and drop the closed record from the in-memory book
    /// so `balance == seed + Σ closed.net_pnl` holds again.
    ///
    /// The close-time cooldowns (`exit_cooldowns`, `asset_last_*`) are left
    /// alone on purpose: they only delay actions conservatively, and the
    /// pre-close values they overwrote are gone — trying to fake them would
    /// just move the drift.
    ///
    /// Returns false (and changes nothing) if the position id is already open,
    /// so a repeated unwind cannot duplicate the row.
    pub fn restore_closed(&mut self, pre_close: OpenPosition, closed: &ClosedPosition) -> bool {
        if self.open.iter().any(|p| p.id == pre_close.id) {
            return false;
        }
        // The deploy line keeps the day's realized PnL inside `DailyLossState`
        // (#173), so the reversal is the exact inverse of `close()`'s credit.
        self.daily.realized_pnl_usd -= closed.net_pnl_usd;
        if let Some(i) = self
            .closed
            .iter()
            .rposition(|c| c.id == closed.id && c.exited_at_ms == closed.exited_at_ms)
        {
            self.closed.remove(i);
        }
        self.open.push(pre_close);
        true
    }

    /// Capacity + cooldown gate (mirrors TS `canOpen`).
    pub fn can_open(
        &self,
        asset: Option<&str>,
        direction: Option<SignalDirection>,
        now_ms: i64,
    ) -> Result<(), String> {
        if self.open.len() >= self.config.max_positions {
            return Err(format!("Max positions ({})", self.config.max_positions));
        }
        if self.daily_loss_tripped() {
            let limit = if self.daily.tripped_limit_usd > Decimal::ZERO {
                self.daily.tripped_limit_usd
            } else {
                self.effective_daily_loss_limit()
            };
            return Err(format!(
                "Daily loss limit (${limit}; realized ${}, day {})",
                self.daily.realized_pnl_usd,
                self.daily.day_index.unwrap_or(0)
            ));
        }
        if self.config.stop_loss_cooldown_sec > 0
            && self.last_stop_loss_at > 0
            && now_ms - self.last_stop_loss_at < self.config.stop_loss_cooldown_sec * 1000
        {
            let left = (self.config.stop_loss_cooldown_sec * 1000
                - (now_ms - self.last_stop_loss_at))
                / 1000
                + 1;
            return Err(format!("SL cooldown: {left}s"));
        }
        if let Some(asset) = asset
            && self.open.iter().any(|p| p.asset == asset)
        {
            return Err(format!("Already in {asset}"));
        }
        if let (Some(asset), Some(direction)) = (asset, direction) {
            let key = cooldown_key(asset, direction);
            if let Some(&last) = self.exit_cooldowns.get(&key)
                && now_ms - last < self.config.exit_cooldown_sec * 1000
            {
                let left = (self.config.exit_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                return Err(format!(
                    "Exit cooldown {asset} {}: {left}s",
                    direction.as_str()
                ));
            }
        }
        if let Some(asset) = asset {
            if self.config.loss_cooldown_sec > 0
                && let Some(&last) = self.asset_last_loss_at.get(asset)
                && now_ms - last < self.config.loss_cooldown_sec * 1000
            {
                let left = (self.config.loss_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                return Err(format!("Loss cooldown {asset}: {left}s"));
            }
            if self.config.asset_cooldown_sec > 0
                && let Some(&last) = self.asset_last_exit_at.get(asset)
                && now_ms - last < self.config.asset_cooldown_sec * 1000
            {
                let left = (self.config.asset_cooldown_sec * 1000 - (now_ms - last)) / 1000 + 1;
                return Err(format!("Asset cooldown {asset}: {left}s"));
            }
        }
        Ok(())
    }
}

// SignalDirection::as_str lives on the shared type (strategy_logic::model) —
// cooldown keys and logs read it from there.

// Keep Side referenced (used by callers wiring exits to orders).
#[allow(dead_code)]
fn _side_used(_: Side) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn params(asset: &str, dir: SignalDirection, entry: Decimal) -> OpenParams {
        OpenParams {
            strategy: "spread_arb".into(),
            asset: asset.into(),
            direction: dir,
            token_id: format!("tok_{asset}"),
            condition_id: "cond".into(),
            entry_price: entry,
            expires_at_ms: 900_000,
            was_maker: true,
            target_exit_price: None,
        }
    }

    /// Open a position AND accrue its entry fill — the two steps production
    /// always performs together (service: `open` then `apply_entry_fill`).
    fn enter(
        pm: &mut PositionManager,
        p: OpenParams,
        role: OrderRole,
        now_ms: i64,
    ) -> OpenPosition {
        let shares = dec!(10);
        let price = p.entry_price;
        let pos = pm.open(p, now_ms);
        let fee = if role.is_maker() {
            Decimal::ZERO
        } else {
            (crate::exit_policy::taker_fee_pct(price) / Decimal::ONE_HUNDRED) * price * shares
        };
        pm.apply_entry_fill(&pos.id, shares, price, fee, role)
            .unwrap()
    }

    fn one_sided_book(ask: Decimal, ts: i64) -> OrderbookSnapshot {
        // F6 regression fixture: NO bids at all, only an ask. The old mid
        // arithmetic turned this into a phantom sellable price of ask/2.
        OrderbookSnapshot::from_levels("tok_BTC".to_string(), vec![], vec![(ask, dec!(100))], ts)
    }

    fn two_sided_book(bid: Decimal, ask: Decimal, ts: i64) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels(
            "tok_BTC".to_string(),
            vec![(bid, dec!(100))],
            vec![(ask, dec!(100))],
            ts,
        )
    }

    /// F6: with no buyer in the book, no exit request may be produced at all —
    /// not from the mid, not from the stale `current_price`. Deadline pressure
    /// (force-exit territory) included.
    #[test]
    fn no_bid_mint_no_exit_request_even_at_deadline() {
        let mut pm = PositionManager::new(PositionConfig::default());
        let p = enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            0,
        );
        // 10 s before expiry: a live bid would fire ForceExit immediately.
        let now = p.expires_at_ms - 10_000;
        let reqs = pm.check_exits(
            &|token| {
                if token == "tok_BTC" {
                    Some(one_sided_book(dec!(0.90), now))
                } else {
                    None
                }
            },
            now,
        );
        assert!(
            reqs.is_empty(),
            "no bid ⇒ no sellable quote ⇒ no exit request"
        );

        // Sanity: the same tick WITH a real bid still exits (gate works both ways).
        let reqs = pm.check_exits(
            &|token| {
                if token == "tok_BTC" {
                    Some(two_sided_book(dec!(0.50), dec!(0.52), now))
                } else {
                    None
                }
            },
            now,
        );
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].exit_price,
            dec!(0.50),
            "exit prices off the live bid"
        );
    }

    /// F6: a stale bid is not a buyer standing here now — an expired book must
    /// not price a forced exit at an old price.
    #[test]
    fn stale_book_does_not_price_a_forced_exit() {
        let mut pm = PositionManager::new(PositionConfig::default());
        let p = enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            0,
        );
        let now = p.expires_at_ms - 10_000;
        // Bid 0.60 captured 2 minutes ago (budget: default 60 s).
        let stale = now - 120_000;
        let reqs = pm.check_exits(
            &|token| {
                if token == "tok_BTC" {
                    Some(two_sided_book(dec!(0.60), dec!(0.62), stale))
                } else {
                    None
                }
            },
            now,
        );
        assert!(reqs.is_empty(), "stale book must not price an exit");
        assert_eq!(pos_book_ts(&pm, &p.id), stale, "last book seen is recorded");

        // The same bid, received NOW, is executable again.
        let reqs = pm.check_exits(
            &|token| {
                if token == "tok_BTC" {
                    Some(two_sided_book(dec!(0.60), dec!(0.62), now))
                } else {
                    None
                }
            },
            now,
        );
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].exit_price, dec!(0.60));
    }

    fn pos_book_ts(pm: &PositionManager, id: &str) -> i64 {
        pm.open_positions()
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.last_book_ts)
            .unwrap_or(0)
    }

    /// F8: the configured fee model drives the close-path (dust) fee. 0.1
    /// unsold share written off at 0.60 taker: legacy 1.2% of price vs official
    /// crypto 0.07*(1-0.6)*100% = 2.8% of price → dust fee differs by
    /// 1.6% * (0.60 * 0.1) = $0.00096.
    #[test]
    fn close_dust_fee_follows_the_fee_model() {
        let dust_fee = |model: crate::exit_policy::FeeModel| -> Decimal {
            let mut pm = PositionManager::new(PositionConfig {
                exit: crate::exit_policy::ExitConfig {
                    fee_model: model,
                    ..Default::default()
                },
                ..Default::default()
            });
            let p = enter(
                &mut pm,
                params("BTC", SignalDirection::Up, dec!(0.4)),
                OrderRole::Maker,
                0,
            );
            // Sell 9.90 of 10 shares (sub-grid remainder stays) as a taker; the
            // partial's own fee is identical in both runs and cancels out.
            let exit_fee = (crate::exit_policy::FeeModel::LegacyQuadratic.fee_pct(dec!(0.6))
                / Decimal::ONE_HUNDRED)
                * dec!(0.6)
                * dec!(9.9);
            pm.apply_exit_fill(&p.id, dec!(9.9), dec!(0.6), exit_fee, OrderRole::Taker)
                .unwrap();
            let closed = pm
                .close(&p.id, dec!(0.60), ExitReason::Manual, false, 1000)
                .unwrap();
            closed.net_pnl_usd
        };
        // The only difference between the two runs is the dust fee schedule.
        // legacy dust: 1.2% * (0.60 * 0.1) = 0.00072
        // crypto dust: 2.8% * (0.60 * 0.1) = 0.00168
        let legacy = dust_fee(crate::exit_policy::FeeModel::LegacyQuadratic);
        let crypto = dust_fee(crate::exit_policy::FeeModel::PolymarketCrypto);
        assert_eq!(
            legacy - crypto,
            dec!(0.00096),
            "net = gross − fees, per model"
        );
    }

    #[test]
    fn open_and_close_pnl_is_net_of_fees() {
        let mut pm = PositionManager::new(PositionConfig::default());
        // Maker entry (no fee), taker exit at 0.6 → gross 2.0 minus the exit fee.
        let p = enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            0,
        );
        assert_eq!(p.shares, dec!(10));
        assert_eq!(p.cost_usd, dec!(4));
        assert_eq!(p.entry_fee_pct, Decimal::ZERO);
        let c = pm
            .close(&p.id, dec!(0.6), ExitReason::TakeProfit, false, 1000)
            .unwrap();
        assert!(c.net_pnl_usd < dec!(2.0));
        assert!(c.net_pnl_usd > dec!(1.9)); // fee is small around 0.6
        assert!(c.was_maker_entry);
        assert!(!c.was_maker_exit);
        assert_eq!(pm.daily_pnl(), c.net_pnl_usd);
        // Net is the ledger's own arithmetic: proceeds − basis − both fees.
        let entry_fee = (c.entry_fee_pct / Decimal::ONE_HUNDRED) * c.entry_price * c.shares;
        let exit_fee = (c.exit_fee_pct / Decimal::ONE_HUNDRED) * c.exit_price * c.shares;
        assert_eq!(c.net_pnl_usd, c.pnl_usd - entry_fee - exit_fee);
    }

    #[test]
    fn capacity_and_same_asset_are_blocked() {
        let cfg = PositionConfig {
            max_positions: 2,
            asset_cooldown_sec: 0,
            loss_cooldown_sec: 0,
            exit_cooldown_sec: 0,
            ..Default::default()
        };
        let mut pm = PositionManager::new(cfg);
        enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            0,
        );
        assert!(
            pm.can_open(Some("BTC"), Some(SignalDirection::Up), 1000)
                .is_err()
        );
        assert!(
            pm.can_open(Some("ETH"), Some(SignalDirection::Up), 1000)
                .is_ok()
        );
        enter(
            &mut pm,
            params("ETH", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            1000,
        );
        assert!(
            pm.can_open(Some("SOL"), Some(SignalDirection::Up), 1000)
                .is_err()
        ); // max positions
    }

    #[test]
    fn loss_sets_asset_cooldown() {
        let cfg = PositionConfig {
            asset_cooldown_sec: 90,
            loss_cooldown_sec: 180,
            ..Default::default()
        };
        let mut pm = PositionManager::new(cfg);
        let p = enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            0,
        );
        pm.close(&p.id, dec!(0.3), ExitReason::StopLoss, false, 10_000)
            .unwrap();
        // Immediately after a loss, re-entry is blocked by asset/loss cooldown.
        assert!(
            pm.can_open(Some("BTC"), Some(SignalDirection::Down), 11_000)
                .is_err()
        );
        // Far in the future it clears.
        assert!(
            pm.can_open(Some("BTC"), Some(SignalDirection::Down), 10_000 + 200_000)
                .is_ok()
        );
    }

    #[test]
    fn daily_loss_limit_blocks_new_positions() {
        let cfg = PositionConfig {
            max_daily_loss_usd: dec!(1),
            asset_cooldown_sec: 0,
            loss_cooldown_sec: 0,
            stop_loss_cooldown_sec: 0,
            ..Default::default()
        };
        let mut pm = PositionManager::new(cfg);
        let p = enter(
            &mut pm,
            params("BTC", SignalDirection::Up, dec!(0.4)),
            OrderRole::Maker,
            0,
        );
        pm.close(&p.id, dec!(0.05), ExitReason::StopLoss, false, 1000)
            .unwrap();
        assert!(pm.daily_pnl() < dec!(-1));
        assert!(
            pm.can_open(Some("ETH"), Some(SignalDirection::Up), 2000)
                .is_err()
        );
    }
}

/// E17 accounting contract: whatever mix of maker/taker roles and partial fills
/// a position goes through, its `net_pnl_usd` is EXACTLY the cash the ledger
/// moved for it — `(proceeds − basis) − entry_fee − exit_fee`. The full
/// end-to-end equality (`balance == seed + Σ netPnl`) is asserted on the real
/// ledger in `service::account_precision_tests`; these pin the arithmetic.
#[cfg(test)]
mod precision_tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn taker_fee(price: Decimal, shares: Decimal) -> Decimal {
        (crate::exit_policy::taker_fee_pct(price) / Decimal::ONE_HUNDRED) * price * shares
    }

    /// Open an empty position shell. Size and basis come from the venue's fills
    /// (`apply_entry_fill`), never from the request, so there is nothing to pass.
    fn open_at(pm: &mut PositionManager, price: Decimal) -> OpenPosition {
        pm.open(
            OpenParams {
                strategy: "s".into(),
                asset: "BTC".into(),
                direction: SignalDirection::Up,
                token_id: "tok".into(),
                condition_id: "cond".into(),
                entry_price: price,
                expires_at_ms: 1_000_000,
                was_maker: false,
                target_exit_price: None,
            },
            0,
        )
    }

    /// The four partial-fill shapes the plan calls out, each with the fee the
    /// ledger would have charged, asserting the position's net matches the cash.
    #[test]
    fn partial_entry_and_exit_flows_reconcile_exactly() {
        for (entry_fills, exit_fills) in [
            (vec![(dec!(1), dec!(0.40))], vec![(dec!(1), dec!(0.60))]),
            (vec![(dec!(5), dec!(0.40))], vec![(dec!(5), dec!(0.60))]),
            (
                vec![(dec!(9.99), dec!(0.40))],
                vec![(dec!(9.99), dec!(0.60))],
            ),
            (vec![(dec!(10), dec!(0.40))], vec![(dec!(10), dec!(0.60))]),
            // Ladders: several fills per leg, mixed prices.
            (
                vec![(dec!(4), dec!(0.40)), (dec!(6), dec!(0.42))],
                vec![(dec!(3), dec!(0.55)), (dec!(7), dec!(0.60))],
            ),
            // Mixed roles: maker in, part maker / part taker out.
            (
                vec![(dec!(10), dec!(0.43))],
                vec![(dec!(4), dec!(0.70)), (dec!(6), dec!(0.95))],
            ),
        ] {
            let mut pm = PositionManager::new(PositionConfig::default());
            let pos = open_at(&mut pm, entry_fills[0].1);

            let mut entry_cost = Decimal::ZERO;
            let mut entry_fee = Decimal::ZERO;
            let mut opened = Decimal::ZERO;
            for (shares, price) in &entry_fills {
                // Alternate role: index 0 taker, later maker — exercises the fold.
                let role = if opened == Decimal::ZERO {
                    OrderRole::Taker
                } else {
                    OrderRole::Maker
                };
                let fee = if role.is_maker() {
                    Decimal::ZERO
                } else {
                    taker_fee(*price, *shares)
                };
                pm.apply_entry_fill(&pos.id, *shares, *price, fee, role)
                    .unwrap();
                entry_cost += price * shares;
                entry_fee += fee;
                opened += shares;
            }

            let mut proceeds = Decimal::ZERO;
            let mut exit_fee = Decimal::ZERO;
            let mut sold = Decimal::ZERO;
            for (i, (shares, price)) in exit_fills.iter().enumerate() {
                let role = if i % 2 == 0 {
                    OrderRole::Taker
                } else {
                    OrderRole::Maker
                };
                let fee = if role.is_maker() {
                    Decimal::ZERO
                } else {
                    taker_fee(*price, *shares)
                };
                pm.apply_exit_fill(&pos.id, *shares, *price, fee, role)
                    .unwrap();
                proceeds += price * shares;
                exit_fee += fee;
                sold += shares;
            }

            let closed = pm
                .close(
                    &pos.id,
                    exit_fills.last().unwrap().1,
                    ExitReason::Manual,
                    false,
                    10,
                )
                .unwrap();

            assert_eq!(closed.shares, opened, "shares = what was opened");
            assert_eq!(closed.cost_usd, entry_cost, "basis comes from the ledger");
            assert_eq!(
                closed.pnl_usd,
                proceeds - entry_cost,
                "gross = Δcash before fees"
            );
            assert_eq!(
                closed.net_pnl_usd,
                proceeds - entry_cost - entry_fee - exit_fee,
                "net must be the ledger's own arithmetic (entry={entry_fills:?} exit={exit_fills:?})"
            );
            assert_eq!(
                closed.dust_shares,
                Decimal::ZERO,
                "fully sold leaves no dust"
            );
            assert_eq!(
                closed.was_maker_entry,
                entry_fee == Decimal::ZERO,
                "maker flag mirrors the fee actually paid"
            );
            assert_eq!(closed.was_maker_exit, exit_fee == Decimal::ZERO);
        }
    }

    /// A sub-grid remainder (0.001 of a share) is written off at the exit price
    /// and charged to net, so cash and the record still agree.
    #[test]
    fn sub_grid_dust_is_written_off_not_left_open() {
        let mut pm = PositionManager::new(PositionConfig::default());
        let pos = open_at(&mut pm, dec!(0.40));
        pm.apply_entry_fill(
            &pos.id,
            dec!(10),
            dec!(0.40),
            Decimal::ZERO,
            OrderRole::Maker,
        )
        .unwrap();
        // Sell everything sellable (10.00) but claim only 9.999 filled.
        pm.apply_exit_fill(
            &pos.id,
            dec!(9.999),
            dec!(0.60),
            Decimal::ZERO,
            OrderRole::Maker,
        )
        .unwrap();
        let closed = pm
            .close(&pos.id, dec!(0.60), ExitReason::Manual, true, 10)
            .unwrap();
        assert_eq!(closed.dust_shares, dec!(0.001));
        // The dust is priced at the exit, so gross covers all 10 shares.
        assert_eq!(
            closed.pnl_usd,
            dec!(0.60) * dec!(10) - dec!(0.40) * dec!(10)
        );
        assert_eq!(closed.net_pnl_usd, closed.pnl_usd);
    }

    /// An exact-grid position sells in full: the old 0.01 flat buffer is gone, so
    /// a 10-share position no longer leaves 0.01 shares stranded forever.
    #[test]
    fn sell_shares_sends_the_whole_exact_grid_position() {
        let mut pm = PositionManager::new(PositionConfig::default());
        let pos = open_at(&mut pm, dec!(0.40));
        pm.apply_entry_fill(
            &pos.id,
            dec!(10),
            dec!(0.40),
            Decimal::ZERO,
            OrderRole::Maker,
        )
        .unwrap();
        assert_eq!(pm.sell_shares(&pos.id), Some(dec!(10)));

        // Off-grid holdings floor down — never oversell.
        pm.apply_entry_fill(
            &pos.id,
            dec!(0.005),
            dec!(0.40),
            Decimal::ZERO,
            OrderRole::Maker,
        )
        .unwrap();
        assert_eq!(pm.sell_shares(&pos.id), Some(dec!(10)));
        assert!(pm.sell_shares(&pos.id).unwrap() <= pm.open_positions()[0].shares);
    }

    /// A pre-E17 snapshot has no recorded flows; they are repaired from the old
    /// fields so a restart mid-flight cannot lose the basis.
    #[test]
    fn legacy_snapshot_flows_are_repaired() {
        let mut pm = PositionManager::new(PositionConfig::default());
        let pos = open_at(&mut pm, dec!(0.40));
        let mut legacy = pm
            .apply_entry_fill(&pos.id, dec!(10), dec!(0.40), dec!(0.01), OrderRole::Taker)
            .unwrap();
        // Simulate a snapshot serialised by the previous version: flows absent,
        // only the old cost_usd/entry_fee_pct/entry_price fields populated.
        legacy.flows = CashFlows::default();
        legacy.cost_usd = dec!(4.0);
        legacy.shares = dec!(10);
        legacy.entry_price = dec!(0.40);
        legacy.entry_fee_pct = taker_fee(dec!(0.40), dec!(10)) / dec!(4.0) * dec!(100);
        let repaired = legacy.flows();
        assert_eq!(repaired.opened_shares, dec!(10));
        assert_eq!(repaired.entry_cost_usd, dec!(4.0));
        assert!(repaired.entry_fee_usd > Decimal::ZERO);
    }
}
