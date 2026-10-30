//! Position manager — open positions, PnL, exit checks, cooldowns, capacity.
//!
//! Rust port of `src/strategies/crypto-hft/positions.ts` (deleted with the Node
//! source layer, `62b16c88`). Decision logic is delegated to the pure
//! `exit_policy` module; this module owns the stateful parts:
//! the open/closed books, daily PnL, and per-asset/direction cooldowns.

use crate::exit_policy::{
    BookView, ExitConfig, ExitState, ExitTickInput, ExitVerdict, book_age_ms, book_is_fresh,
    decide_exit_verdict, effective_stop_pct, executable_bid, pnl_pct, reference_price,
    update_exit_state,
};
use crate::model::{
    AccountId, ExitReason, OrderRole, OrderbookSnapshot, SignalDirection, default_account_id,
};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

mod book;
mod daily;
mod exits;
mod open;

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
    /// Σ maker rebate the venue paid on this position's MAKER fills (modeled
    /// in replay: the official rule pays a maker 20% of the market's taker-fee
    /// pool pro-rata by maker share — `apply_delta_effects` books the per-fill
    /// approximation and the ledger credit together, so `net` below and the
    /// cash ledger cannot disagree about it). Snapshots written before the
    /// field existed carry the serde default (zero): the money is simply not
    /// claimed for pre-model positions.
    pub maker_rebate_usd: Decimal,
    /// Σ shares bought (the closed trade's size).
    pub opened_shares: Decimal,
    /// Σ shares sold across all exit fills.
    pub sold_shares: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PositionConfig {
    pub exit: ExitConfig,
    pub max_positions: usize,
    /// #363: how many positions may be open at once on ONE asset. This
    /// replaces the hardcoded one-per-asset gate (the binary
    /// "already in this asset?" check) with a count the execution policy
    /// (and `--max-positions-per-asset`) can raise — the @Almach replay
    /// shows the chain running CONCURRENT positions on one asset across
    /// rounds, so the old gate was a benchmark distortion (§ #355 point 1).
    /// The default `1` is the historical behaviour byte for byte: the same
    /// entries, the same "Already in {asset}" refusal text.
    pub max_positions_per_asset: u32,
    /// Absolute daily-loss cap in USD. `0` = no absolute cap: the relative cap
    /// below is then the whole budget. Kept because an operator may want a hard
    /// dollar number regardless of account size.
    pub max_daily_loss_usd: Decimal,
    /// Daily-loss cap as a percentage of the day's OPENING cash equity — the
    /// book [`roll_daily`](PositionManager::roll_daily) was told it came from,
    /// re-anchored when that basis changes mid-day (P1 #235). `0` = off. This is
    /// the default budget precisely because an absolute default is meaningless
    /// across account sizes: the old hard-coded 200 USD was an unbounded budget
    /// for a 4.8 USDC book (P0 #173).
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
    /// #464: whether the daily-loss breaker judges on the day's NET mark —
    /// realized loss PLUS the open positions' unrealized loss — or (when
    /// `false`) on realized alone, the pre-#464 behaviour byte for byte. `true`
    /// by default: a position that has bled 20% of the day's budget on paper is
    /// the loss the breaker exists to fence, and waiting for the realized close
    /// turns the cap into a settle-late admission ticket.
    #[serde(default = "default_true")]
    pub daily_loss_marks_unrealized: bool,
    pub stop_loss_cooldown_sec: i64,
    pub exit_cooldown_sec: i64,
    pub asset_cooldown_sec: i64,
    pub loss_cooldown_sec: i64,
    /// Issue #464: the operator's explicit `[[risk.ladder]]` as a per-position
    /// exit overlay. Empty (the default) = no ladder — the exit sweep below
    /// behaves exactly as it always has. Non-empty: when a position's own
    /// verdict is a TakeProfit or StopLoss, the ordered ladder takes over that
    /// verdict — the next un-executed step whose `at_pct` the position's pnl%
    /// has reached sells `close_ratio` of the held shares (floored to the
    /// venue's 0.01 grid), and a step's `move_stop_to` raises the protective
    /// stop floor to that absolute price. One step fires per tick; steps never
    /// skip ahead of an untriggered predecessor.
    ///
    /// `LadderStep` serializes on the arbitration face only; the config file's
    /// rows remain the single source and are copied in at assembly (main.rs),
    /// so this field is a runtime hand-off and not a parse surface.
    pub exit_ladder: Vec<crate::arbitration::LadderStep>,
}

/// #464: the serde default for [`PositionConfig::daily_loss_marks_unrealized`]
/// — the net-mark reading of "daily loss" ships ON.
fn default_true() -> bool {
    true
}

impl Default for PositionConfig {
    fn default() -> Self {
        Self {
            exit: ExitConfig::default(),
            max_positions: 2,
            // #363: the historical one-per-asset gate, now a number.
            max_positions_per_asset: 1,
            // Off by default; the relative budget below carries the protection.
            max_daily_loss_usd: Decimal::ZERO,
            // 20% of the day's opening cash equity. Meaningful on any account
            // size, unlike a fixed dollar default.
            max_daily_loss_equity_pct: Decimal::from(20),
            daily_pnl_path: None,
            stop_suppression_repeat_sec: 30,
            // #464: the net-mark breaker (realized + unrealized) is the shipped
            // reading of "daily loss"; `false` restores the realized-only cap.
            daily_loss_marks_unrealized: true,
            stop_loss_cooldown_sec: 180,
            exit_cooldown_sec: 60,
            asset_cooldown_sec: 90,
            loss_cooldown_sec: 180,
            exit_ladder: Vec::new(),
        }
    }
}

/// Milliseconds in one UTC day.
pub const DAY_MS: i64 = 86_400_000;

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

/// How far the book may move within one UTC day before the day's base is
/// treated as belonging to a different account (P1 #235).
///
/// Same-day cash moves are fees and realized PnL, and the loss breaker already
/// bounds the latter at `max_daily_loss_equity_pct` — 50% is far above that, so
/// normal trading can never trip this, and far below the dry-seed-versus-real
/// gap (orders of magnitude) this exists to catch. It covers the case the
/// recorded basis cannot: the label did not change, but the account size did
/// (a re-seeded dry run, a deposit or a withdrawal).
fn same_day_rebase_deviation_pct() -> Decimal {
    Decimal::from(50)
}

/// Which book the day's opening equity was measured from (P1 #235).
///
/// The percentage cap is a share of an ACCOUNT, and this system trades two of
/// them: the simulated book a locally-settling run trades (`dry_seed_balance`)
/// and the venue-reconciled book a live run trades. A day stamped against one
/// and then measured against the other is measured against a number from a
/// different world — 20% of a $10 000 dry seed is a $2 000 budget on a $50 live
/// account, which is not a breaker at all. So the base records where it came
/// from, and a basis change re-anchors it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EquityBasis {
    /// The simulated book: `Mode::Dry` and `Mode::ReadOnly` both settle against
    /// it (see `Mode::settles_locally`).
    Dry,
    /// The venue-reconciled live book.
    Live,
}

impl EquityBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dry => "dry",
            Self::Live => "live",
        }
    }
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
    /// not shrink the budget it is being measured against. The one thing that
    /// does move it is [`Self::equity_basis`] changing under it (P1 #235).
    pub opening_equity_usd: Decimal,
    /// Which book [`Self::opening_equity_usd`] was measured from. `None` = not
    /// recorded (a state file written before P1 #235), which is NOT the same as
    /// "whatever is running now": an unrecorded basis can never prove the base
    /// still belongs to the book in front of it, so it re-anchors like any
    /// other change.
    pub equity_basis: Option<EquityBasis>,
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
            equity_basis: None,
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
    /// #268: same held exit, different reason — the book itself is past its
    /// age budget, so its bid is not a buyer standing here now. A thin market
    /// and a dead feed look identical from inside `executable_bid` (both return
    /// zero) but they need opposite responses: a thin market is a price to
    /// wait out, an outage is an operator problem. The `book_age_ms` on the
    /// event is what separates them.
    StaleBook,
}

/// How many book-freshness budgets a position may stay unpriceable before the
/// held exit stops being routine and is escalated (#268, item 2).
///
/// Deliberately a constant and not a knob: `N x max_book_age_sec` is the
/// statement the issue asks for — "this position has been unable to act for
/// longer than the data budget it is judged on, several times over" — and a
/// configurable version of it would be one more number nobody has a value for.
/// Three budgets is ~180 s at the shipped default: long enough that a genuinely
/// thin market never pages anyone, short enough that an outage is called out
/// while the position is still open.
const UNPRICEABLE_ESCALATION_BUDGETS: i64 = 3;

/// One exit the F6 gate withheld from becoming an order — "should have
/// triggered" made visible to review and to the panel (P0 #177, F6).
#[derive(Debug, Clone, PartialEq)]
pub struct SuppressedStopEvent {
    pub cause: StopSuppressionCause,
    /// The exit rule that wanted out, when one did. `None` for the outage
    /// escalation, which is about a position that could not be priced at all
    /// rather than about a rule that fired (#268).
    pub reason: Option<ExitReason>,
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
    /// For the held-exit causes: the price the exit was actually JUDGED on —
    /// the protective stop's own reference when the stop is what fired (the
    /// ask-side price of a bid-less book, #225), otherwise the profit side's
    /// reference (#267), and the position's last valuation when neither
    /// resolved.
    pub mid: Decimal,
    pub pnl_pct_at_bid: Decimal,
    pub pnl_pct_at_mid: Decimal,
    pub stop_pct: Decimal,
    /// How old the book was at the moment the exit was withheld. `None` = no
    /// book at all, or one whose age cannot be judged (`timestamp <= 0`).
    /// This is what tells "the feed is dead" apart from "the market is thin"
    /// (#268): both leave `bid` at zero.
    pub book_age_ms: Option<i64>,
    /// How long this position has been continuously unpriceable, ending at
    /// `now_ms`. `0` = this is the first tick it could not be priced.
    pub unpriceable_for_ms: i64,
    /// True once [`unpriceable_for_ms`](Self::unpriceable_for_ms) passed
    /// `N x max_book_age_sec`: the suppression is no longer a routine hold but
    /// an outage worth a risk alert (#268, item 2).
    pub escalated: bool,
    pub now_ms: i64,
}

impl SuppressedStopEvent {
    /// The audit-trail line: what the exit rule wanted, what it judged on, and
    /// why no order followed.
    ///
    /// The wording names the rule that actually fired (#267). It used to say
    /// "protective stop suppressed" for every cause, which read as "your stop
    /// was held back" even when the rule that fired was a trailing stop — the
    /// one thing a reviewer must not have to guess about.
    pub fn message(&self) -> String {
        // "protective stop" is a claim about WHICH rule fired, so it is only
        // made when a protective stop is what fired.
        let what = match self.reason {
            Some(ExitReason::StopLoss) => "protective stop SUPPRESSED".to_string(),
            Some(r) => format!("exit rule {r:?} withheld"),
            None => "position UNPRICEABLE".to_string(),
        };
        match self.cause {
            StopSuppressionCause::WickGuard => format!(
                "{what} (wick guard): {} {} entry {} bid {} (-{}%) mid {} ({}%) stop {}% — \
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
            StopSuppressionCause::StaleBook => format!(
                "{what} (stale book{}): {} {} entry {} bid {} ({}%) judged {} ({}%) — the book is \
                 {}ms past its age budget, so that bid is not a buyer standing here now and no \
                 order was sent; the position is held until a fresh book arrives or expiry settles \
                 it",
                self.escalation_note(),
                self.position_id,
                self.asset,
                self.entry_price,
                self.bid,
                self.pnl_pct_at_bid,
                self.mid,
                self.pnl_pct_at_mid,
                self.book_age_ms.unwrap_or(0)
            ),
            StopSuppressionCause::NoExecutableQuote if self.bid > Decimal::ZERO => format!(
                "{what} (stale quote): {} {} entry {} bid {} ({}%) stop {}% — the exit rule fired, \
                 but the book was past its staleness budget, so that bid could not price a SELL; \
                 the position is held until a fresh quote appears or expiry settles it",
                self.position_id,
                self.asset,
                self.entry_price,
                self.bid,
                self.pnl_pct_at_bid,
                self.stop_pct
            ),
            StopSuppressionCause::NoExecutableQuote => format!(
                "{what} (no executable bid): {} {} entry {} judged {} ({}%) stop {}% — the exit \
                 rule fired, but the book showed no bid at all, so no order was sent; the position \
                 is held until a buyer appears or expiry settles it",
                self.position_id,
                self.asset,
                self.entry_price,
                self.mid,
                self.pnl_pct_at_mid,
                self.stop_pct
            ),
        }
    }

    /// The escalation clause of the audit line: empty until the position has
    /// been unpriceable past the total-inaction budget (#268, item 2).
    fn escalation_note(&self) -> String {
        if self.escalated {
            format!(
                ", ESCALATED: unpriceable {}s",
                self.unpriceable_for_ms / 1000
            )
        } else {
            String::new()
        }
    }
}

/// The durable side file holding the day's loss budget. Same convention as the
/// position log's `.recon` watermark: one small file, best-effort writes, a
/// missing/corrupt file never blocks startup (memory stays authoritative).
///
/// The file holds ONE envelope with every account's day (the daily budget is
/// per-account — `max_daily_drawdown_usd` sits in the risk matrix's ACCOUNT
/// family, and one account's losses must never burn a sibling's budget). A
/// file written before per-account days holds a bare `DailyLossState`; that
/// migrates to the `default` account's bucket on load.
struct DailyLossStore {
    path: std::path::PathBuf,
}

/// The persisted envelope: account id → its day state.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct DailyLossFile {
    pub accounts: std::collections::BTreeMap<String, DailyLossState>,
}

impl DailyLossStore {
    fn new(path: impl Into<String>) -> Self {
        Self {
            path: std::path::PathBuf::from(path.into()),
        }
    }

    fn save(&self, accounts: &std::collections::BTreeMap<AccountId, DailyLossState>) {
        let file = DailyLossFile {
            accounts: accounts
                .iter()
                .map(|(id, st)| (id.as_str().to_string(), st.clone()))
                .collect(),
        };
        match serde_json::to_string_pretty(&file) {
            Ok(text) => {
                // #463: atomic rewrite. This file IS the "restart cannot launder
                // today's losses" guarantee — a truncate-in-place write is the
                // one crash window that defeats it.
                if !crate::jsonl::rewrite_atomic(&self.path, text.as_bytes()) {
                    tracing::warn!(path = %self.path.display(), "daily-loss persist failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "daily-loss serialize failed"),
        }
    }

    fn load(&self) -> std::collections::BTreeMap<AccountId, DailyLossState> {
        let Some(text) = std::fs::read_to_string(&self.path).ok() else {
            return Default::default();
        };
        // New envelope first; a bare pre-per-account state migrates into the
        // `default` account's bucket (single-book deployments had exactly one).
        if let Ok(file) = serde_json::from_str::<DailyLossFile>(&text) {
            return file
                .accounts
                .into_iter()
                .map(|(id, st)| (AccountId::from(id.as_str()), st))
                .collect();
        }
        match serde_json::from_str::<DailyLossState>(&text) {
            Ok(st) => {
                tracing::info!(
                    path = %self.path.display(),
                    "daily-loss state migrated to per-account shape (default account)"
                );
                [(AccountId::from("default"), st)].into_iter().collect()
            }
            Err(e) => {
                // #463: unreadable is NOT "fresh day". A silent zero hands the
                // full daily-loss budget back to a session whose earlier losses
                // are sitting right there in a half-written file — the exact
                // laundering this store exists to prevent. Trading continues
                // (in-memory state stays authoritative), but the operator is
                // told loudly and the conservative boot budget stands until the
                // file is repaired or explicitly removed.
                tracing::error!(
                    error = %e,
                    path = %self.path.display(),
                    "daily-loss state UNREADABLE — NOT starting a fresh day; \
                     today's realized losses may exceed what this session can see. \
                     Repair or delete the file before trusting the loss budget."
                );
                Default::default()
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
    /// E28 (§9.2): the account the position belongs to. Defaulted on read so
    /// a 0.2 snapshot lands in the `default` book without a migration; the
    /// entry path writes it explicitly from the originating order (read old,
    /// write new).
    #[serde(default = "default_account_id")]
    pub account_id: AccountId,
    /// #427: which venue the position trades on — copied from the market that
    /// filled the entry (serde default keeps old snapshots loading as the
    /// undeclared single-venue world). Pure DATA for the panel's Venue column;
    /// nothing branches on the value.
    #[serde(default)]
    pub venue: String,
    /// The condition's OTHER outcome token, stamped at entry from the market
    /// that filled it (the same stamp the venue field rides). The dry
    /// settlement ladder reads it as the counter-evidence key: a decisive
    /// partner bid names our leg the loser. Empty on snapshots written before
    /// the field existed — those keep the old withhold behavior (no partner,
    /// no counter verdict) and stay on the manual path.
    #[serde(default)]
    pub partner_token_id: String,
    /// F6: when the exit path last received a book for this token (ms epoch).
    /// A forced exit may consult it to refuse pricing off a stale quote; it is
    /// `0` until the first book arrives (serde default keeps old snapshots
    /// loading as "no book seen since restart").
    #[serde(default)]
    pub last_book_ts: i64,
    /// B1a: the round-length family the position was opened under, in
    /// seconds. `0` = pre-multi-family legacy snapshot (serde default keeps
    /// old positions.jsonl lines loading unchanged and byte-identical on
    /// rewrite); B1b stamps it at the open path and backfills lazily at the
    /// expiry/force-close consumers. The authoritative source is the
    /// market's `round_duration_sec`;
    /// [`OpenPosition::effective_round_duration_sec`] resolves the
    /// three-state fallback (own > market declaration > global).
    #[serde(default, skip_serializing_if = "position_zero_i64")]
    pub round_duration_sec: i64,
    /// The round slot the entry order traded (B1b grid coordinate: slot N
    /// covers [N·dur, (N+1)·dur)). Expiry is judged by SLOT MATCHING, not a
    /// countdown: the position's round is over once the current grid position
    /// passes slot N — the same formula as the order-side
    /// `force_close_order`. `0` = pre-slot legacy snapshot (serde default
    /// keeps old positions.jsonl lines byte-identical); those fall back to
    /// the `expires_at_ms` countdown, which the open path anchors to
    /// `(slot+1)·dur` anyway.
    #[serde(default, skip_serializing_if = "position_zero_i64")]
    pub round_slot: i64,
    pub state: ExitState,
    /// Actual cash flows since the position opened. Snapshots written before E17
    /// carry the serde default (all zero) and are repaired by [`OpenPosition::flows`].
    #[serde(default)]
    pub flows: CashFlows,
}

/// B1a serde companion: `0` (= undeclared / legacy snapshot) never reaches
/// the wire, so a positions.jsonl line written after this PR stays
/// byte-identical with the pre-B1a shape whenever the field still carries
/// the default.
fn position_zero_i64(v: &i64) -> bool {
    *v == 0
}

impl OpenPosition {
    pub fn high_pnl_pct(&self) -> Decimal {
        self.state.high_pnl_pct
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

    /// B1a: the position's effective round length, resolved three-state (read
    /// only — no write here): the position's OWN stamp when it carries one
    /// (B1b's write path at open), else the market's declared duration when
    /// the market spoke (#354's `round_duration_sec`), else the configured
    /// global grid. `0` from every source = the caller has no clock fact at
    /// all; B1b's consumers (expiry sweep, force-close, held-position view,
    /// pair-chase) decide what a 0 means at their own gates.
    pub fn effective_round_duration_sec(&self, market_decl: Option<i64>, global_sec: i64) -> i64 {
        if self.round_duration_sec > 0 {
            return self.round_duration_sec;
        }
        if let Some(d) = market_decl.filter(|d| *d > 0) {
            return d;
        }
        global_sec
    }

    /// B1a: idempotent lazy backfill — write `sec` only when the field is
    /// still the legacy `0`. An existing non-0 stamp is never overwritten
    /// (the first family a record was admitted under stays authoritative),
    /// so calling this on every sweep pass is free and stable. The kernel's
    /// whole state lives under the single-threaded Core lock
    /// (`ipc/server.rs` serializes every verb and sweep), so the write-back
    /// rides the ordinary field-assignment path — no new lock.
    pub fn backfill_round_duration(&mut self, sec: i64) {
        if self.round_duration_sec == 0 {
            self.round_duration_sec = sec;
        }
    }
}

/// #463: `Serialize`/`Deserialize` because a close credential (which carries
/// the closed record for unwinding) is journaled to disk — a FAILED arriving
/// after a restart must be able to reverse every ledger the close touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Maker rebate the venue paid on this trade's MAKER fills (modeled in
    /// replay; serde default keeps pre-rebate lines loading). Already INSIDE
    /// `net_pnl_usd` — carrying it separately is the attribution a maker-vs-
    /// taker comparison reads, never an amount to add on top.
    #[serde(default, with = "crate::decimal")]
    pub maker_rebate_usd: Decimal,
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
    /// E28 (§9.2): the account the trade settled in — carried from the position
    /// (which carried it from the order), so per-account realized PnL is a field
    /// read, never a strategy-name heuristic.
    pub account_id: AccountId,
    /// #427: the venue the trade's market listed on — carried from the
    /// position, pure DATA for the panels' Venue column.
    pub venue: String,
}

/// The result of one MERGE (task 2.2): complete UP+DOWN share-pairs of one
/// condition burned into their on-chain collateral. Each merged leg leaves at
/// $0.50/share, so the pair's two exit notionals sum to exactly the $1.00 the
/// ledger credits per pair — the trade records and the cash never disagree.
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    pub condition_id: String,
    /// E28 (§9.2): the account whose wallet owned the pair — the collateral
    /// credit lands here, never in another book.
    pub account_id: AccountId,
    /// #647: the venue the burned pairs sit on — carried from the legs' own
    /// execution-venue stamp (the same fact the executor drain routes by), so
    /// the merge request reaches the executor that can run the CTF merge.
    /// Empty when the legs were opened untagged (the pre-routing world).
    pub venue: String,
    /// Complete share-pairs merged (the smaller side's share count).
    pub pairs_merged: Decimal,
    /// Collateral collected: `pairs_merged` × $1.00.
    pub cash_credited_usd: Decimal,
    /// Legs fully consumed by the merge (closed as `ExitReason::Merge`).
    pub closed: Vec<ClosedPosition>,
    /// Shares left open on the longer leg(s) — they stay for settlement.
    pub unpaired_shares: Decimal,
}

pub struct OpenParams {
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    /// E28 (§9.2): the account this position spends — stamped from the entry
    /// order, so per-account PnL/positions are a field read, never a guess.
    pub account_id: AccountId,
    /// #427: the venue the entry market listed on — empty when undeclared.
    pub venue: String,
    /// The condition's other outcome token, stamped alongside the venue from
    /// the same market lookup. Empty when no market was found (or by older
    /// callers): the dry ladder treats empty as "no counter evidence".
    pub partner_token_id: String,
    /// The order's limit price; the position's actual entry price is the basis
    /// per share once its fills land (see [`PositionManager::apply_entry_fill`]).
    pub entry_price: Decimal,
    pub expires_at_ms: i64,
    /// Role the entry order was REQUESTED as; informational at open time (the
    /// fee comes from the fill's own role in [`PositionManager::apply_entry_fill`]).
    pub was_maker: bool,
    pub target_exit_price: Option<Decimal>,
    /// B1b: the round-length family the entry was admitted under, resolved by
    /// the opening path off the token's market and the global grid. `0` (older
    /// callers) opens the position unstamped: B1a's lazy backfill at the
    /// expiry/force-close consumers may still stamp it later, and the B1a
    /// readers fall back through the market declaration as before.
    pub round_duration_sec: i64,
    /// The entry order's round slot (grid coordinate). `0` = no slot fact
    /// (older callers / legacy snapshots): expiry judgment falls back to the
    /// `expires_at_ms` countdown.
    pub round_slot: i64,
}

#[derive(Debug, Clone)]
pub struct ExitRequest {
    pub position_id: String,
    pub reason: ExitReason,
    pub exit_price: Decimal,
    pub use_maker: bool,
    /// Issue #464: shares to close when this request becomes a SELL. `None` =
    /// the whole sellable position (every historical request — the field is
    /// `None` at every other construction site), so the no-ladder path is
    /// byte-identical. A ladder step carries `close_ratio × current shares`
    /// (floored to the share grid) to sell in tranches.
    pub close_shares: Option<Decimal>,
}

pub struct PositionManager {
    config: PositionConfig,
    open: Vec<OpenPosition>,
    closed: Vec<ClosedPosition>,
    next_id: u64,
    /// Each account's realized-loss budget for the UTC day (P0 #173). The
    /// budget is PER-ACCOUNT (operator verdict 2026-10-25): the cap sits in
    /// the risk matrix's ACCOUNT family (`max_daily_drawdown_usd`), and one
    /// account's losses must never burn a sibling's budget — the single-slot
    /// state counted the whole process's day against whichever account was
    /// entering. `daily_pnl_for` is a view of this state rather than a
    /// separate field, so persistence cannot drift.
    daily_by_account: std::collections::BTreeMap<AccountId, DailyLossState>,
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
    /// When each position entered its current run of "cannot be priced" (#268,
    /// item 2). Cleared the moment a tick prices it again, and when it closes,
    /// so the value is always the START of the current run and never a
    /// lifetime total.
    unpriceable_since: std::collections::HashMap<String, i64>,
    /// Issue #390 (replay nofast): the instant this position's exit ladder last
    /// RAN, so an interval-scheduled pass can compensate the per-tick counter
    /// ratchets the skipped ticks would have walked. Written by
    /// `check_exits_interval` only; every other writer of exit state is a
    /// per-event path whose clocks stay event-driven.
    exit_ladder_last_run: std::collections::HashMap<String, i64>,
    /// Issue #464: how many `[[risk.ladder]]` steps each position has already
    /// executed (the index of the NEXT step to arm). Ladder steps fire in
    /// order — one per tick at most, and never a step whose predecessor has
    /// not triggered — so a momentary spike cannot skip levels and dump the
    /// whole position in one tick. Keyed by position id; dropped when the
    /// position closes or the open book is replaced.
    ladder_steps_done: std::collections::HashMap<String, usize>,
    /// Issue #464: the protective-stop FLOOR each position's executed ladder
    /// steps have raised (`move_stop_to`). The ladder overlay fires a full
    /// remaining-position stop when the pnl% reaches this floor OR the
    /// policy's own stop — `move_stop_to` only ever moves the stop UP, never
    /// down. Dropped with the position, like the ledger above.
    ladder_stop_floor: std::collections::HashMap<String, Decimal>,
    /// Per-strategy trailing-stop overrides (the strategies' own declaration,
    /// `EngineStrategy::wants_trailing_exit`): `Some(true)` banks profit even
    /// when the kernel-wide trailing is off, `Some(false)` refuses it even
    /// when on, absent = follow the kernel value in `config.exit`. Written by
    /// the service at registration and re-read per ladder tick — a declaration
    /// is a policy fact, not per-position state, so it needs no persistence
    /// beyond the strategies themselves.
    trailing_overrides: std::collections::HashMap<String, bool>,
    /// Per-strategy stop widths for hold-to-settlement strategies (the
    /// strategies' own declaration, `EngineStrategy::holds_to_settlement`):
    /// strategy name → the WIDE containment stop (`ExitConfig::holder_stop_pct`)
    /// its positions' protective stop runs on. Absent = the ordinary
    /// `stop_loss_pct` ladder, byte-identical to history. Written by the same
    /// registration beat as `trailing_overrides` — a declaration is a policy
    /// fact, not per-position state.
    holder_stop_overrides: std::collections::HashMap<String, Decimal>,
}

fn cooldown_key(asset: &str, direction: SignalDirection) -> String {
    format!("{asset}_{}", direction.as_str())
}

/// The fee the close path charges on the unsold remainder it prices itself.
///
/// Pure over `schedule` on purpose (#234, item 5). The active schedule is
/// process-wide and set-once (`exit_policy::ACTIVE_FEE_SCHEDULE`), so no test can
/// price two schedules through the close path in one process — but the DIFFERENCE
/// between them is what the dust test exists to prove, and a test that restates
/// the arithmetic proves only that the restatement is stable. Taking the schedule
/// as a parameter lets the test drive BOTH curves through this, the production
/// expression, while [`PositionManager::close`] passes the one in force.
///
/// Maker exits pay nothing: the schedule charges takers only.
fn dust_fee_for(
    schedule: crate::exit_policy::FeeSchedule,
    exit_price: Decimal,
    dust_notional: Decimal,
    was_maker: bool,
) -> Decimal {
    if was_maker {
        return Decimal::ZERO;
    }
    (schedule.fee_pct(exit_price) / Decimal::ONE_HUNDRED) * dust_notional
}

impl PositionManager {
    pub fn new(config: PositionConfig) -> Self {
        let daily_store = config.daily_pnl_path.clone().map(DailyLossStore::new);
        // A persisted budget must gate entries BEFORE the first tick, so it is
        // loaded here rather than lazily: a restart on a day whose cap is
        // already spent must not accept one entry first.
        let daily = daily_store.as_ref().map(|s| s.load()).unwrap_or_default();
        Self {
            config,
            open: Vec::new(),
            closed: Vec::new(),
            next_id: 1,
            daily_by_account: daily,
            daily_store,
            last_stop_loss_at: 0,
            exit_cooldowns: Default::default(),
            asset_last_exit_at: Default::default(),
            asset_last_loss_at: Default::default(),
            suppressed_stops: Vec::new(),
            suppressed_stop_count: 0,
            last_suppression_at: Default::default(),
            unpriceable_since: Default::default(),
            exit_ladder_last_run: Default::default(),
            ladder_steps_done: Default::default(),
            ladder_stop_floor: Default::default(),
            trailing_overrides: Default::default(),
            holder_stop_overrides: Default::default(),
        }
    }

    pub fn set_config(&mut self, config: PositionConfig) {
        let path_changed = config.daily_pnl_path != self.config.daily_pnl_path;
        self.config = config;
        if path_changed {
            self.daily_store = self.config.daily_pnl_path.clone().map(DailyLossStore::new);
            // A different budget file is a different budget: re-seed from it,
            // keeping the in-memory state when the new file has none.
            if let Some(loaded) = self.daily_store.as_ref().map(|s| s.load()) {
                self.daily_by_account = loaded;
            }
        }
    }

    /// Refresh the strategies' trailing declarations (the service calls this
    /// whenever the registry changes — registration, unregistration, enable).
    /// A whole-map replace keeps stale entries of unregistered strategies from
    /// lingering; strategies that declared nothing are simply absent, which
    /// IS the "follow the kernel value" state.
    pub fn set_trailing_overrides(&mut self, overrides: std::collections::HashMap<String, bool>) {
        self.trailing_overrides = overrides;
    }

    /// Refresh the hold-to-settlement strategies' stop widths (the service
    /// calls this on the SAME registry-change beat as
    /// [`PositionManager::set_trailing_overrides`]). A whole-map replace keeps
    /// stale entries of unregistered strategies from lingering; strategies
    /// that declared nothing are simply absent, which IS the ordinary
    /// `stop_loss_pct` ladder.
    pub fn set_holder_stop_overrides(
        &mut self,
        overrides: std::collections::HashMap<String, Decimal>,
    ) {
        self.holder_stop_overrides = overrides;
    }

    pub fn open_positions(&self) -> &[OpenPosition] {
        &self.open
    }

    /// B1b: lazy family backfill over the whole open book — every still-legacy
    /// (`round_duration_sec == 0`) row gets the family its token's market
    /// resolves to, resolved by the CALLER (the core owns the market lookup;
    /// the manager only owns rows). Idempotent by construction:
    /// [`OpenPosition::backfill_round_duration`] never overwrites a non-0
    /// stamp, so calling this on every sweep pass is free and stable.
    pub fn backfill_round_families(&mut self, family_of: impl Fn(&str) -> i64) {
        for p in &mut self.open {
            let fam = family_of(&p.token_id);
            if fam > 0 {
                p.backfill_round_duration(fam);
            }
        }
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
        // #390: the restored book replaces the in-memory one wholesale —
        // ladder run stamps of wiped rows must not leak into the new ids.
        self.exit_ladder_last_run.clear();
        // #464: same wholesale-replacement rule — a restored position starts
        // with no ladder steps executed and no raised stop floor (the snapshot
        // carries neither; re-executing a step at worst sells another tranche
        // at the same profitable level, never a stop below the policy's own).
        self.ladder_steps_done.clear();
        self.ladder_stop_floor.clear();
    }
    pub fn closed_positions(&self) -> &[ClosedPosition] {
        &self.closed
    }

    /// issue 561 账户维重置：把 `account` 的 open 与 closed 行从内存簿里
    /// 拿掉，其他账户的行一个不动。返回 (移除的 open 数, 移除的 closed 数)。
    ///
    /// closed 半边是裁决 3 的根因修复之一：重置把 trade 文件清成空后，
    /// `recent_trades` 会兜底读这个内存 closed 表——不摘除目标账户的行，
    /// 「真实余额 = 本金 + 净利」就会把已被重置抹掉的净利重新折回余额
    /// （$1000 → $991.31 的复活链，issue 561）。持久侧（position_db）由
    /// 调用方用 save(剩余全部账户行) 落盘，这里只管内存权威。
    pub fn remove_account_positions(&mut self, account: &str) -> (usize, usize) {
        let open_before = self.open.len();
        self.open.retain(|p| p.account_id.as_str() != account);
        let closed_before = self.closed.len();
        self.closed.retain(|c| c.account_id.as_str() != account);
        (
            open_before - self.open.len(),
            closed_before - self.closed.len(),
        )
    }

    /// Test seam: land one CLOSED row directly in the book, bypassing
    /// open→fill→close. The #364 preview replay needs historical facts
    /// without a full fill simulation; the production path always arrives
    /// through [`Self::close`], which owns the day-budget fold.
    #[cfg(test)]
    pub fn push_closed_for_test(&mut self, closed: ClosedPosition) {
        self.closed.push(closed);
    }
}

// SignalDirection::as_str lives on the shared type (strategy_logic::model) —
// cooldown keys and logs read it from there.

#[cfg(test)]
mod tests;

/// E17 accounting contract: whatever mix of maker/taker roles and partial fills
/// a position goes through, its `net_pnl_usd` is EXACTLY the cash the ledger
/// moved for it — `(proceeds − basis) − entry_fee − exit_fee`. The full
/// end-to-end equality (`balance == seed + Σ netPnl`) is asserted on the real
/// ledger in `service::account_precision_tests`; these pin the arithmetic.
#[cfg(test)]
mod precision_tests;
