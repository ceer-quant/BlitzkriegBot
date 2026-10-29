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

    /// The account's day-realized PnL (its OWN bucket — a sibling account's
    /// day never moves this number).
    pub fn daily_pnl_for(&self, account: &str) -> Decimal {
        self.daily_bucket(account).realized_pnl_usd
    }

    /// The process-wide day realized — every account's bucket summed. This is
    /// the SYSTEMIC fact (the `global_kill_switch_loss_usd` cap reads it); the
    /// per-account caps read [`Self::daily_pnl_for`] instead.
    pub fn daily_pnl_total(&self) -> Decimal {
        self.daily_by_account
            .values()
            .map(|s| s.realized_pnl_usd)
            .sum()
    }

    fn daily_bucket(&self, account: &str) -> &DailyLossState {
        static EMPTY: std::sync::OnceLock<DailyLossState> = std::sync::OnceLock::new();
        self.daily_by_account
            .get(&AccountId::from(account))
            .unwrap_or_else(|| EMPTY.get_or_init(DailyLossState::default))
    }

    /// The day's realized-loss budget as the panel/CLI report it — ONE
    /// account's day (the caller names whose; the snapshot serves the display
    /// account's and carries the whole map beside it).
    pub fn daily_state_for(&self, account: &str) -> DailyLossState {
        self.daily_bucket(account).clone()
    }

    /// Every account's day, for views that must show all books at once.
    pub fn daily_states(&self) -> &std::collections::BTreeMap<AccountId, DailyLossState> {
        &self.daily_by_account
    }

    /// #363: the account's net realized PnL over the TRAILING HOUR, read
    /// straight off the closed-trade list (`exited_at_ms` is the close's own
    /// timestamp, so a restart loses the window until the first fresh close —
    /// the same session-scope the streak counter has; a policy that needs the
    /// last hour across restarts should gate on `consecutive_losses` instead,
    /// which is persisted). Per-account by field, the E28 way — never a
    /// strategy-name heuristic.
    pub fn recent_pnl_1h_for(&self, account_id: &str, now_ms: i64) -> Decimal {
        let window = 3_600_000i64;
        self.closed
            .iter()
            .filter(|c| c.account_id.as_str() == account_id && now_ms - c.exited_at_ms <= window)
            .map(|c| c.net_pnl_usd)
            .sum()
    }

    /// The open positions' unrealized PnL (USD, negative = paper loss), each
    /// position marked `(current_price − entry_price) × shares` off its last
    /// known reference price. Positions with no shares, no mark (`current_price
    /// ≤ 0`) or an absurd share count contribute 0 — never a fabricated profit
    /// or loss.
    ///
    /// `ponytail:` the mark is the LAST KNOWN price: `current_price` is
    /// refreshed from live books per tick, but a book that has gone quiet
    /// leaves the last value standing and there is no staleness gate on it
    /// (Issue #464 considered one; upgrade path = a `last_book_ts`-age check
    /// here, which `OpenPosition` already carries).
    pub fn unrealized_pnl_usd(&self) -> Decimal {
        self.open
            .iter()
            .filter(|p| p.shares > Decimal::ZERO && p.current_price > Decimal::ZERO)
            .map(|p| (p.current_price - p.entry_price) * p.shares)
            .sum()
    }

    /// ONE account's open book, same mark discipline as
    /// [`Self::unrealized_pnl_usd`] — the per-account daily breaker judges its
    /// own paper loss, never a sibling's.
    pub fn unrealized_pnl_usd_for(&self, account: &str) -> Decimal {
        self.open
            .iter()
            .filter(|p| p.account_id.as_str() == account)
            .filter(|p| p.shares > Decimal::ZERO && p.current_price > Decimal::ZERO)
            .map(|p| (p.current_price - p.entry_price) * p.shares)
            .sum()
    }

    /// The number the daily-loss breaker judges on (#464): the day's realized
    /// loss plus — when [`PositionConfig::daily_loss_marks_unrealized`] is on —
    /// the open book's unrealized loss. The realized-only reading ships OFF;
    /// setting that flag `false` reproduces the pre-#464 input exactly. The
    /// persisted [`DailyLossState::realized_pnl_usd`] is NOT this number: the
    /// unrealized half is a JUDGEMENT input, never an accounting entry.
    /// Per-account: the account's own realized, the account's own paper.
    fn daily_drawdown_input(&self, account: &str) -> Decimal {
        let realized = self.daily_bucket(account).realized_pnl_usd;
        if self.config.daily_loss_marks_unrealized {
            realized + self.unrealized_pnl_usd_for(account)
        } else {
            realized
        }
    }

    /// The cap for ONE account's day (its own opening equity base).
    pub fn effective_daily_loss_limit_for_public(&self, account: &str) -> Decimal {
        self.effective_daily_loss_limit_for(account)
    }

    /// The cap for ONE account's day (its own opening equity base).
    fn effective_daily_loss_limit_for(&self, account: &str) -> Decimal {
        let abs = self.config.max_daily_loss_usd;
        let pct = self.config.max_daily_loss_equity_pct;
        let opening = self.daily_bucket(account).opening_equity_usd;
        let rel = if pct > Decimal::ZERO && opening > Decimal::ZERO {
            opening * pct / Decimal::ONE_HUNDRED
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

    /// True when the day's net loss (see [`Self::daily_drawdown_input`])
    /// reached the effective cap. Sticky for the rest of the day so a later
    /// winning trade cannot re-open entries.
    pub fn daily_loss_tripped_for(&self, account: &str) -> bool {
        let daily = self.daily_bucket(account);
        if daily.tripped {
            return true;
        }
        let limit = self.effective_daily_loss_limit_for(account);
        limit > Decimal::ZERO && self.daily_drawdown_input(account) <= -limit
    }

    /// Roll the budget over when `now_ms` crosses into a new UTC day, seeding
    /// the new day's opening equity from `equity_usd` (the caller's cash
    /// equity: the position manager cannot read the ledger itself).
    ///
    /// `basis` says WHICH book that equity came from (P1 #235) — the caller
    /// reads it off the mode, because the same ledger balance means a different
    /// thing in dry and in live. Within a day it is what tells a re-anchored
    /// budget apart from the same account still trading, so it is a parameter
    /// rather than a default: a caller that forgets it would silently keep the
    /// fail-open behaviour this exists to close.
    ///
    /// Returns `Some` exactly when a new day opened, so the caller can put the
    /// old day's result in the audit trail. The FIRST call only stamps the day
    /// (nothing to close yet) and reports it, so the budget's start is visible.
    /// A same-day re-anchor is NOT a new day and reports `None`; it is announced
    /// by its own warning, and by the trip alert when it freezes the day.
    pub fn roll_daily(
        &mut self,
        account: &str,
        now_ms: i64,
        equity_usd: Decimal,
        basis: EquityBasis,
    ) -> Option<DailyRoll> {
        let today = utc_day_index(now_ms);
        if self
            .daily_by_account
            .get(&AccountId::from(account))
            .map(|d| d.day_index)
            .unwrap_or(None)
            == Some(today)
        {
            self.reconcile_same_day_base(account, now_ms, equity_usd, basis);
            return None;
        }
        let daily = self
            .daily_by_account
            .entry(AccountId::from(account))
            .or_default();
        let previous_day_index = daily.day_index;
        let previous_realized = daily.realized_pnl_usd;
        let previous_tripped = daily.tripped;
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
        let carried_at = daily.tripped_at_ms;
        let carried_limit = daily.tripped_limit_usd;
        *daily = DailyLossState {
            day_index: Some(today),
            realized_pnl_usd: carried_pnl,
            opening_equity_usd: equity_usd.max(Decimal::ZERO),
            equity_basis: Some(basis),
            tripped: carried_trip,
            tripped_at_ms: if carried_trip { carried_at } else { 0 },
            tripped_limit_usd: if carried_trip {
                carried_limit
            } else {
                Decimal::ZERO
            },
            tripped_reported: false,
            opened_at_ms: now_ms,
        };
        // A REAL UTC-day boundary for this account refreshes the strategy
        // cooldowns; a brand-new account's first stamp (previous_day_index
        // None) must NOT — it would wipe siblings' same-day exit cooldowns
        // mid-day. (The single-slot state cleared these unconditionally: one
        // book, one day.)
        if previous_day_index.is_some() {
            self.last_stop_loss_at = 0;
            self.exit_cooldowns.clear();
        }
        self.persist_daily();
        Some(DailyRoll {
            day_index: today,
            previous_day_index,
            previous_realized_pnl_usd: previous_realized,
            opening_equity_usd: self.daily_bucket(account).opening_equity_usd,
            limit_usd: self.effective_daily_loss_limit_for(account),
            previous_tripped,
        })
    }

    /// Keep the day's opening equity honest WITHIN the day (P1 #235).
    ///
    /// The cap is a share of the account the day opened on, and only one thing
    /// may move that base mid-day: a base that was measured against a DIFFERENT
    /// book. Without this, a dry run that stamped the day with its $10 000 seed
    /// leaves a live $50 account carrying a $2 000 cap — and since
    /// [`Self::effective_daily_loss_limit`] takes the TIGHTER of the absolute
    /// and the relative cap, an inflated base inflates the cap: the breaker
    /// cannot trip. That is the fail-open direction, so it is closed here.
    ///
    /// Two conditions re-anchor, and both are clamped to the safe direction:
    ///
    /// * the basis changed (dry↔live — including a state file that never
    ///   recorded one, which cannot prove it belongs to the book in front of
    ///   it), or
    /// * the book moved by more than [`same_day_rebase_deviation_pct`], the
    ///   "same label, different account size" case the basis alone misses.
    ///
    /// A re-anchor may only TIGHTEN. A larger book keeps the day's smaller base,
    /// because re-anchoring upward would hand the day a budget it never had —
    /// the very hole this closes. That is also why a same-basis restart at
    /// (roughly) the same equity changes nothing and says nothing: a restart
    /// must not disturb how much of the day's budget is already spent.
    fn reconcile_same_day_base(
        &mut self,
        account: &str,
        now_ms: i64,
        equity_usd: Decimal,
        basis: EquityBasis,
    ) {
        if equity_usd <= Decimal::ZERO {
            // No account size to measure against (a live process whose balance
            // has not arrived yet): keep whatever base the day has.
            return;
        }
        let daily = self
            .daily_by_account
            .entry(AccountId::from(account))
            .or_default();
        let old_equity = daily.opening_equity_usd;
        let old_basis = daily.equity_basis;
        if old_equity <= Decimal::ZERO {
            // Stamped before the account size was known: adopt today's book.
            daily.opening_equity_usd = equity_usd;
            daily.equity_basis = Some(basis);
            self.persist_daily();
            return;
        }
        let basis_changed = old_basis != Some(basis);
        let moved_pct = ((equity_usd - old_equity).abs() / old_equity) * Decimal::ONE_HUNDRED;
        if !basis_changed && moved_pct <= same_day_rebase_deviation_pct() {
            return;
        }
        if equity_usd >= old_equity {
            // The other book is bigger: re-anchoring would LOOSEN the day's cap,
            // so the day keeps the tighter base it already has.
            return;
        }
        daily.opening_equity_usd = equity_usd;
        daily.equity_basis = Some(basis);
        let day_index = daily.day_index.unwrap_or(0);
        let realized = daily.realized_pnl_usd;
        let limit = self.effective_daily_loss_limit_for(account);
        let breach = self.daily_drawdown_input(account);
        let breached = limit > Decimal::ZERO && breach <= -limit;
        if breached {
            // The tightened cap is already spent. Latch the trip HERE rather
            // than letting the next tick rediscover it, so the day cannot read
            // as healthy in between, and re-arm the report: the number the
            // operator was shown has moved. Exits are untouched either way —
            // the trip freezes entries (see `can_open`).
            if let Some(daily) = self.daily_by_account.get_mut(&AccountId::from(account)) {
                if !daily.tripped {
                    daily.tripped = true;
                    daily.tripped_at_ms = now_ms;
                }
                daily.tripped_limit_usd = limit;
                daily.tripped_reported = false;
            }
        }
        let old_basis_str = old_basis.map(EquityBasis::as_str).unwrap_or("unrecorded");
        tracing::warn!(
            account = %account,
            day = day_index,
            old_equity = %old_equity,
            new_equity = %equity_usd,
            old_basis = old_basis_str,
            new_basis = basis.as_str(),
            realized = %realized,
            limit = %limit,
            tripped = breached,
            "daily loss budget re-anchored within the UTC day: opening equity {} ({}) -> {} ({}){}",
            old_equity,
            old_basis_str,
            equity_usd,
            basis.as_str(),
            if breached {
                " — the tightened cap is already spent, so entries are frozen for the rest of the day (exits are unaffected)"
            } else {
                ""
            }
        );
        self.persist_daily();
    }

    /// Every account's un-announced trip, latched and consumed. The caller
    /// announces one line per account — a sibling account's freeze must not
    /// hide behind another's.
    pub fn take_daily_trips(&mut self) -> Vec<(AccountId, DailyTrip)> {
        let mut trips = Vec::new();
        let accounts: Vec<AccountId> = self.daily_by_account.keys().cloned().collect();
        for account in accounts {
            if !self.daily_loss_tripped_for(account.as_str()) {
                continue;
            }
            // The latch cap is computed BEFORE the mutable borrow (it reads
            // the bucket's opening equity through the same map).
            let latch_limit = self.effective_daily_loss_limit_for(account.as_str());
            let (day_index, realized, opening, limit, at_ms) = {
                let daily = self
                    .daily_by_account
                    .get_mut(&account)
                    .expect("bucket exists (keys come from the map)");
                if !daily.tripped {
                    // Detected here rather than only in `close` (a restored budget can
                    // arrive already breached): latch it so the panel agrees.
                    daily.tripped = true;
                    daily.tripped_at_ms = daily.opened_at_ms;
                    daily.tripped_limit_usd = latch_limit;
                }
                if daily.tripped_reported {
                    continue;
                }
                daily.tripped_reported = true;
                (
                    daily.day_index.unwrap_or(0),
                    daily.realized_pnl_usd,
                    daily.opening_equity_usd,
                    daily.tripped_limit_usd,
                    daily.tripped_at_ms,
                )
            };
            self.persist_daily();
            trips.push((
                account,
                DailyTrip {
                    day_index,
                    realized_pnl_usd: realized,
                    limit_usd: limit,
                    opening_equity_usd: opening,
                    at_ms,
                },
            ));
        }
        trips
    }

    fn persist_daily(&self) {
        if let Some(store) = self.daily_store.as_ref() {
            store.save(&self.daily_by_account);
        }
    }

    /// Latch the breaker if the day's realized loss just reached the cap. Called
    /// from [`close`](Self::close) so the freeze and its persistence happen with
    /// the loss, not one tick later — against the CLOSED position's account.
    fn note_daily_drawdown(&mut self, account: &str, now_ms: i64) {
        let limit = self.effective_daily_loss_limit_for(account);
        if limit > Decimal::ZERO
            && self.daily_drawdown_input(account) <= -limit
            && !self.daily_bucket(account).tripped
            && let Some(daily) = self.daily_by_account.get_mut(&AccountId::from(account))
        {
            daily.tripped = true;
            daily.tripped_at_ms = now_ms;
            daily.tripped_limit_usd = limit;
            tracing::warn!(
                account = %account,
                day = daily.day_index.unwrap_or(0),
                realized = %daily.realized_pnl_usd,
                limit = %limit,
                equity = %daily.opening_equity_usd,
                "daily loss limit reached — new entries frozen for this account until the next UTC day"
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
            account_id: p.account_id,
            venue: p.venue,
            partner_token_id: p.partner_token_id,
            last_book_ts: 0,
            // B1b: stamped from the opening path's family resolution — `0`
            // only when the caller had no family fact (older callers).
            round_duration_sec: p.round_duration_sec,
            // The entry order's grid slot, stamped by the opening path. `0`
            // = the caller had no slot fact; expiry then falls back to the
            // `expires_at_ms` countdown (legacy behavior).
            round_slot: p.round_slot,
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

    /// Accrue one MAKER fill's modeled rebate onto the position's cash flows
    /// (the entry side; a maker EXIT fill's rebate is accrued by
    /// [`Self::apply_exit_fill`]'s caller through the same method). The caller
    /// credits the ledger in the same breath, so the rebate is money the trade
    /// record AND the cash ledger both show.
    ///
    /// A negative `shares` (a FAILED rollback) reverses by the same rule the
    /// other accruals use — the rebate the venue never paid must not stay on
    /// the books (floored at zero, like every flow, so a rollback past a
    /// settled boundary cannot mint a negative rebate).
    pub fn accrue_maker_rebate(&mut self, position_id: &str, rebate_usd: Decimal) {
        if rebate_usd == Decimal::ZERO {
            return;
        }
        if let Some(pos) = self.open.iter_mut().find(|p| p.id == position_id) {
            pos.flows.maker_rebate_usd =
                (pos.flows.maker_rebate_usd + rebate_usd).max(Decimal::ZERO);
        }
    }

    /// B1b: idempotent lazy backfill of the position's round-length family —
    /// the manager-flavoured wrapper over
    /// [`OpenPosition::backfill_round_duration`]. A non-0 stamp is never
    /// overwritten (the first family a record was admitted under stays
    /// authoritative), so calling this on every fill / sweep pass is free and
    /// stable. The whole state lives under the single-threaded Core lock
    /// (`ipc/server.rs` serializes every verb and sweep), so the write-back
    /// rides the ordinary field-assignment path — no new lock.
    pub fn backfill_round_duration(&mut self, position_id: &str, sec: i64) {
        if sec <= 0 {
            return;
        }
        if let Some(p) = self.open.iter_mut().find(|p| p.id == position_id) {
            p.backfill_round_duration(sec);
        }
    }

    /// Accrue one EXIT fill (a partial close). Returns the position's new share
    /// count, or None if the position is unknown. A negative `shares` reverses an
    /// earlier exit fill (rolled back by the venue) and returns its basis.
    ///
    /// `maker_rebate_usd` is the modeled rebate this exit fill's MAKER role
    /// earned (zero for a taker exit / when the replay models no rebate). It
    /// folds into the same flows the close reads, so the trade record's net
    /// carries it exactly once.
    pub fn apply_exit_fill(
        &mut self,
        position_id: &str,
        shares: Decimal,
        price: Decimal,
        fee_usd: Decimal,
        role: OrderRole,
        maker_rebate_usd: Decimal,
    ) -> Option<Decimal> {
        let pos = self.open.iter_mut().find(|p| p.id == position_id)?;
        pos.flows.sold_shares = (pos.flows.sold_shares + shares).max(Decimal::ZERO);
        pos.flows.proceeds_usd = (pos.flows.proceeds_usd + price * shares).max(Decimal::ZERO);
        pos.flows.exit_fee_usd = (pos.flows.exit_fee_usd + fee_usd).max(Decimal::ZERO);
        pos.flows.maker_rebate_usd =
            (pos.flows.maker_rebate_usd + maker_rebate_usd).max(Decimal::ZERO);
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
            let view = book.map(BookView::from_snapshot);
            Self::valuate_one(pos, view.as_ref(), now_ms, &cfg);
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
    fn valuate_one(pos: &mut OpenPosition, book: Option<&BookView>, now_ms: i64, cfg: &ExitConfig) {
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
    pub fn valuate(&mut self, books: &dyn Fn(&str) -> Option<BookView>, now_ms: i64) {
        let cfg = self.config.exit.clone();
        for pos in self.open.iter_mut() {
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
        }
    }

    /// Fast-path valuation (replay `backtest_fast`, all-holder books):
    /// re-value ONLY positions whose token is in `dirty_tokens` — a book
    /// actually arrived for it since the last pass. Every other position
    /// keeps its previous marks, provably what the full pass produced:
    /// `valuate_one` is a pure function of (book, now, cfg) and the book is
    /// unchanged between arrivals, so a quiet-tick re-run reproduces the
    /// same `current_price` (only the tick-count ratchet state stands
    /// still — unobservable, the fast path never consults the holder
    /// ladder). The Decimal-heavy pass over hundreds of idle positions
    /// stops running on every 50 ms maintenance tick.
    pub fn valuate_dirty_tokens(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        dirty_tokens: &std::collections::HashSet<String>,
    ) {
        if dirty_tokens.is_empty() {
            return;
        }
        let cfg = self.config.exit.clone();
        for pos in self.open.iter_mut() {
            if !dirty_tokens.contains(&pos.token_id) {
                continue;
            }
            let book = books(&pos.token_id);
            Self::valuate_one(pos, book.as_ref(), now_ms, &cfg);
        }
    }

    /// Evaluate every open position; returns exit requests for those that hit a
    /// rule. Updates exit state from the book first so the decision and the
    /// recorded HWM share the same quote.
    pub fn check_exits(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
    ) -> Vec<ExitRequest> {
        self.check_exits_inner(books, now_ms, false, 0, &std::collections::HashSet::new())
    }

    /// Issue #390 (replay scheduling): refresh ONLY the display reference
    /// marks — `current_price` / `prev_price` / `last_book_ts` — without
    /// touching the exit state (the counter ratchet in particular). The base
    /// kernel's every-tick valuate advances these marks on every tick and its
    /// ladder runs right after, so a consumer priced off `current_price`
    /// between the two passes sees the fresh mark; the merged replay pass
    /// keeps that ordering visible by refreshing marks separately:
    ///
    ///   * on a SKIP tick, only dirty tokens — the base advanced every mark
    ///     from the same (unchanged) book a quiet re-run would read, so
    ///     refreshing just the tokens whose book changed reproduces it;
    ///   * on a RUN tick, every position — the ladder's own state update
    ///     runs after this and must observe the same marks the base's
    ///     valuate fed it (strategy-signal exits price off `current_price`).
    ///
    /// Deliberately NOT the ladder's `valuate_one`: that would advance the
    /// counter ratchet the interval backfill already owns.
    pub fn refresh_marks(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        all: bool,
        dirty_tokens: &std::collections::HashSet<String>,
    ) {
        for pos in self.open.iter_mut() {
            if !all && !dirty_tokens.contains(&pos.token_id) {
                continue;
            }
            let book = books(&pos.token_id);
            let val = reference_price(book.as_ref(), pos.current_price);
            if val > Decimal::ZERO && pos.current_price != val {
                pos.prev_price = pos.current_price;
                pos.current_price = val;
            }
            if let Some(b) = book {
                pos.last_book_ts = if b.timestamp > 0 { b.timestamp } else { now_ms };
            }
        }
    }

    /// Issue #390 (replay nofast): [`check_exits`] on a deadline schedule.
    ///
    /// `run_now` forces the ladder to run this tick (first pass, dirty token,
    /// deadline); otherwise it is skipped and the caller keeps the previous
    /// verdicts. `tick_boundary_ms` is the replay maintenance grid — the
    /// skipped-tick count the counter ratchet compensates over. Tokens in
    /// `dirty_tokens` (books that arrived since the last pass) must run.
    ///
    /// Output contract: the sequence of (books × ticks) the every-tick pass
    /// observes is partitioned by this pass into "runs" and "skips", and a
    /// skip is exactly a re-observation of the previous run's inputs at a
    /// later clock — its only durable effect is the counter ratchets, which
    /// the interval backfill reproduces (`update_exit_state_interval`). The
    /// timestamps every command and report carries stay `now_ms` — the tick
    /// the verdict actually fires on.
    pub fn check_exits_interval(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        run_now: bool,
        interval_mode: bool,
        tick_boundary_ms: i64,
        dirty_tokens: &std::collections::HashSet<String>,
    ) -> Vec<ExitRequest> {
        if !run_now {
            return Vec::new();
        }
        self.check_exits_inner(books, now_ms, interval_mode, tick_boundary_ms, dirty_tokens)
    }
    /// The shared ladder body. `interval_mode` selects the #390 spelling: the
    /// per-position state update is the merged maintenance update — ONE call
    /// per run at the DOUBLE (2×) backfill rate, because the base kernel's
    /// every-tick valuate pass and every-tick ladder each advance the same
    /// counter ratchet once per tick, and this single run replaces both (the
    /// per-position last-run ledger of the first #390 spelling is gone: two
    /// independent 1× backfills over one shared counter disagree with the
    /// base's joint walk on latch-mid-gap and reset-walk corners). The legacy
    /// spelling keeps `valuate_one` and never touches a ledger.
    ///
    /// The interval branch reuses the legacy tail VERBATIM through
    /// `finish_exit_tick` (the push/report section below it); only the state
    /// update differs (`update_exit_state_interval_2x`).
    fn check_exits_inner(
        &mut self,
        books: &dyn Fn(&str) -> Option<BookView>,
        now_ms: i64,
        interval_mode: bool,
        tick_boundary_ms: i64,
        _dirty_tokens: &std::collections::HashSet<String>,
    ) -> Vec<ExitRequest> {
        let cfg = self.config.exit.clone();
        let mut out = Vec::new();
        let mut withheld: Vec<SuppressedStopEvent> = Vec::new();
        for pos in self.open.iter_mut() {
            // The strategy's own trailing declaration rides ON TOP of the
            // kernel-wide config for THIS position's verdict; every other knob
            // stays shared. Cheap: an absent override short-circuits to the
            // borrowed config via an early match arm.
            let overridden;
            let pos_cfg = match self.trailing_overrides.get(&pos.strategy) {
                None => &cfg,
                Some(on) if *on == cfg.trailing_enabled => &cfg,
                Some(on) => {
                    overridden = ExitConfig {
                        trailing_enabled: *on,
                        ..cfg.clone()
                    };
                    &overridden
                }
            };
            let book = books(&pos.token_id);
            if interval_mode {
                crate::exit_policy::update_exit_state_interval_2x(
                    &mut pos.state,
                    pos.entry_price,
                    book.as_ref(),
                    now_ms,
                    pos_cfg,
                    self.exit_ladder_last_run
                        .get(&pos.id)
                        .copied()
                        .unwrap_or(now_ms),
                    tick_boundary_ms,
                );
                self.exit_ladder_last_run.insert(pos.id.clone(), now_ms);
            } else {
                Self::valuate_one(pos, book.as_ref(), now_ms, pos_cfg);
            }
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
            // #268: the age is kept as well as the verdict on it, because
            // "stale" and "thin" need telling apart downstream and the age is
            // the only thing that can. `book_is_fresh` is the ONE spelling of
            // the budget question (an unknown age counts as fresh — see its
            // doc); a missing book has nothing to age and is left to the
            // judgement-side bounds.
            let book_age_ms = book.as_ref().and_then(|b| book_age_ms(b, now_ms));
            let book_fresh = book.as_ref().is_none_or(|b| book_is_fresh(b, now_ms, &cfg));
            let priceable = exit_price > Decimal::ZERO && book_fresh;
            // #268 item 2: how long this position has been unable to act. The
            // run starts at the first unpriceable tick and is cleared by the
            // first tick that can price it, so what the escalation below reads
            // is a continuous outage and never a lifetime total.
            let unpriceable_for_ms = if priceable {
                self.unpriceable_since.remove(&pos.id);
                0
            } else {
                let since = *self
                    .unpriceable_since
                    .entry(pos.id.clone())
                    .or_insert(now_ms);
                now_ms - since
            };
            let escalated =
                unpriceable_for_ms >= UNPRICEABLE_ESCALATION_BUDGETS * cfg.max_book_age_sec * 1_000;
            // A held exit names its reason: a stale book and a thin one are the
            // same zero to `executable_bid`, and only the age separates them.
            let hold_cause = if book_fresh {
                StopSuppressionCause::NoExecutableQuote
            } else {
                StopSuppressionCause::StaleBook
            };
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
                    close_shares: None,
                });
                continue;
            }

            // The verdict carries back a protective stop the wick guard
            // withheld, so "should have triggered" is reportable rather than
            // silent (P0 #177).
            let ExitVerdict {
                decision,
                suppressed_stop,
                stop_reference,
            } = decide_exit_verdict(ExitTickInput {
                entry_price: pos.entry_price,
                book: book
                    .as_ref()
                    .map(|b| b as &dyn crate::exit_policy::BookScalarView),
                fallback_price: Some(pos.current_price),
                time_left_sec,
                hold_sec,
                state: &pos.state,
                now_ms,
                cfg: pos_cfg,
            });
            if let Some(s) = suppressed_stop {
                withheld.push(SuppressedStopEvent {
                    cause: StopSuppressionCause::WickGuard,
                    reason: Some(ExitReason::StopLoss),
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
                    book_age_ms,
                    unpriceable_for_ms,
                    escalated,
                    now_ms,
                });
            }
            // The held-exit report: an exit was warranted and no order could be
            // sent for it. Two things file one — a rule that fired with no buyer
            // to sell to at any price we can name (#225, #267), and a position
            // that has been unpriceable past the budget (#268, item 2). They
            // differ in ONE field, whether a rule named the reason, so they are
            // built once here: a field added for one path cannot go missing from
            // the other.
            let held_exit = |reason: Option<ExitReason>| {
                // What the exit was JUDGED on is the stop's own reference
                // whenever there is one (#225): a bid-less book whose ask
                // collapsed can only be judged on that ask, and `current_price`
                // never moved off the entry there — reporting it would have said
                // "flat" about the one case this report exists for. With no stop
                // reference (#267) the profit side's own judgement is what fired,
                // and `current_price` is the reference it read.
                let judged = stop_reference.map_or(pos.current_price, |r| r.price);
                SuppressedStopEvent {
                    cause: hold_cause,
                    reason,
                    position_id: pos.id.clone(),
                    token_id: pos.token_id.clone(),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    entry_price: pos.entry_price,
                    bid: exit_price,
                    mid: judged,
                    // A zero bid is no pnl to report: `pnl_pct` would call it
                    // -100%, which reads as a wipeout rather than as a market
                    // with no buyer in it.
                    pnl_pct_at_bid: if exit_price > Decimal::ZERO {
                        pnl_pct(exit_price, pos.entry_price)
                    } else {
                        Decimal::ZERO
                    },
                    pnl_pct_at_mid: pnl_pct(judged, pos.entry_price),
                    stop_pct: effective_stop_pct(cfg.stop_loss_pct, time_left_sec, &cfg),
                    book_age_ms,
                    unpriceable_for_ms,
                    escalated,
                    now_ms,
                }
            };
            // Issue #464: the operator's explicit ladder runs on EVERY priceable
            // tick — not only where the policy's own TP/SL verdict fired —
            // because its stop floor (raised by `move_stop_to`) must patrol
            // quiet ticks too: a +2.5% drift below a raised 0.42 floor over a
            // 0.40 entry fires nothing in the policy, and the floor would be
            // dead. A TP/SL verdict that DID fire is absorbed (the ladder owns
            // this position's profit-taking discipline — it either acted or is
            // exhausted); every other verdict, and every no-ladder run, keeps
            // the original single ExitRequest below, byte-identical.
            let ladder_owns_verdicts = !self.config.exit_ladder.is_empty();
            if ladder_owns_verdicts && priceable {
                let ladder = self.config.exit_ladder.clone();
                let stop_pct =
                    effective_stop_pct(cfg.stop_loss_pct, time_left_sec, &self.config.exit);
                let verdict = match &decision {
                    Some(d)
                        if matches!(d.reason, ExitReason::TakeProfit | ExitReason::StopLoss) =>
                    {
                        Some((d.reason, d.use_maker))
                    }
                    _ => None,
                };
                let done = self.ladder_steps_done.entry(pos.id.clone()).or_insert(0);
                let floor = self
                    .ladder_stop_floor
                    .entry(pos.id.clone())
                    .or_insert(Decimal::ZERO);
                let ladder_fired = if let Some(req) = Self::apply_exit_ladder_step(
                    pos,
                    &ladder,
                    done,
                    floor,
                    stop_pct,
                    exit_price,
                    verdict,
                    cfg.maker_first_exit_enabled,
                ) {
                    out.push(req);
                    true
                } else {
                    false
                };
                match &decision {
                    // Absorbed whether the ladder acted or is exhausted.
                    Some(d)
                        if matches!(d.reason, ExitReason::TakeProfit | ExitReason::StopLoss) =>
                    {
                        continue;
                    }
                    // A non-ladder verdict yields this tick to the ladder's own
                    // request if there was one; otherwise it keeps its request.
                    Some(d) if !ladder_fired => out.push(ExitRequest {
                        position_id: pos.id.clone(),
                        reason: d.reason,
                        exit_price,
                        use_maker: d.use_maker,
                        close_shares: None,
                    }),
                    _ => {}
                }
            } else if let Some(d) = decision {
                if priceable {
                    out.push(ExitRequest {
                        position_id: pos.id.clone(),
                        reason: d.reason,
                        exit_price,
                        use_maker: d.use_maker,
                        close_shares: None,
                    });
                } else {
                    // The rule fired but there is no buyer to sell to at any
                    // price we can name — hold, and report the held exit so it
                    // reaches review instead of dying in memory.
                    withheld.push(held_exit(Some(d.reason)));
                }
            } else if escalated {
                // #268 item 2: no rule fired, but this position has not been
                // priceable for longer than the data budget repeated N times.
                // That is not a quiet market — it is a position that cannot
                // act at all, and it is exactly the state the operator had no
                // signal for. Reported through the SAME withheld-exit channel
                // the panel already turns into a risk alert; the throttle
                // (`stop_suppression_repeat_sec`) keeps it from becoming a
                // per-tick storm.
                withheld.push(held_exit(None));
            }
        }
        for ev in withheld {
            self.note_suppressed_stop(ev);
        }
        out
    }

    /// Issue #464: the exit ladder overlay — the ordered `[[risk.ladder]]`
    /// steps over this position's own TP/SL verdict. Runs ONLY when a ladder
    /// is configured and the underlying verdict is a TakeProfit or StopLoss;
    /// the caller then suppresses the verdict's own whole-position request.
    ///
    /// A free function rather than a method on purpose: the exit sweep walks
    /// `self.open` through `iter_mut()`, so a `&mut self` call inside the loop
    /// is a second borrow of the manager. Everything the overlay touches is
    /// passed in as disjoint slices instead (`done`/`floor` are the position's
    /// own entries in the manager's ledgers, default-seeded by the caller).
    ///
    /// Semantics, in order:
    ///
    /// 1. The protective stop is judged FIRST, on the raised floor: the
    ///    effective stop price is `max(policy stop, ladder floor)` — a
    ///    `move_stop_to` only ever lifts the stop, never lowers it. A breach
    ///    closes the ENTIRE remaining position (a stop is not a tranche) and
    ///    the reason stays `StopLoss`.
    /// 2. Otherwise the next un-executed step (`*done`) is the only one that
    ///    may fire — no skipping ahead of an untriggered predecessor, one
    ///    step per tick. When the position's pnl% has reached that step's
    ///    `at_pct`, it sells `close_ratio × current shares`, floored to the
    ///    venue's 0.01 share grid; a floor of zero (a dust remainder) leaves
    ///    the step un-executed and pushes nothing this tick.
    /// 3. A fired step with `move_stop_to` above the current floor raises it
    ///    before the request goes out, and `*done` advances.
    ///
    /// The trigger reads the SAME price the verdict judged on — the
    /// executable bid (`exit_price`) — so the overlay and the policy cannot
    /// disagree about what the market said. `None` = this tick's verdict is
    /// absorbed by the ladder (the fired step was dust, or the next step has
    /// not triggered yet) and no request is produced.
    /// `verdict` is the policy's own (reason, use_maker) when a TP/SL verdict
    /// fired this tick — the step inherits it. On a quiet tick (None) the step
    /// derives its own identity: a profit step IS a take-profit (maker-first
    /// like the policy's), a loss-side step (negative `at_pct`) IS a stop
    /// (taker, always exitable).
    #[allow(clippy::too_many_arguments)]
    fn apply_exit_ladder_step(
        pos: &mut OpenPosition,
        ladder: &[crate::arbitration::LadderStep],
        done: &mut usize,
        floor: &mut Decimal,
        stop_pct: Decimal,
        exit_price: Decimal,
        verdict: Option<(ExitReason, bool)>,
        maker_first: bool,
    ) -> Option<ExitRequest> {
        // (1) The raised stop: policy's effective stop price vs. the ladder's
        // absolute floor, compared at PRICE so the two sources cannot drift.
        let policy_stop_price = pos.entry_price * (Decimal::ONE - stop_pct / Decimal::ONE_HUNDRED);
        let stop_price = (*floor).max(policy_stop_price);
        if pnl_pct(exit_price, pos.entry_price) <= -stop_pct || exit_price <= stop_price {
            tracing::info!(
                position = %pos.id,
                token = %pos.token_id,
                stop_price = %stop_price,
                bid = %exit_price,
                "exit ladder stop floor breached: closing the remaining position"
            );
            return Some(ExitRequest {
                position_id: pos.id.clone(),
                reason: ExitReason::StopLoss,
                exit_price,
                use_maker: false,
                close_shares: None,
            });
        }

        // (2) The next un-executed step. Steps before it already fired; a
        // later step never jumps the queue.
        let Some(step) = ladder.get(*done) else {
            // The ladder is exhausted; the verdict that woke this tick is
            // absorbed (the ladder owns this position's TP/SL discipline).
            return None;
        };
        let pnl = pnl_pct(exit_price, pos.entry_price);
        let triggered = if step.at_pct >= Decimal::ZERO {
            pnl >= step.at_pct
        } else {
            pnl <= step.at_pct
        };
        if !triggered {
            return None;
        }

        // (3) The tranche: close_ratio of the shares still held, floored to
        // the venue grid so the SELL can never oversell. A dust remainder
        // holds the step open — it re-arms on the next tick (the grid can
        // only round down, so a step that floors to zero today stays zero
        // until shares change).
        let tranche = floor_to_grid(pos.shares * step.close_ratio);
        if tranche <= Decimal::ZERO {
            tracing::warn!(
                position = %pos.id,
                step = *done,
                close_ratio = %step.close_ratio,
                shares = %pos.shares,
                "exit ladder step floored to an untradeable tranche; held open"
            );
            return None;
        }

        // (4) The step's stop raise, recorded BEFORE the request so a failure
        // downstream cannot leave the floor behind the price that armed it.
        if let Some(raise) = step.move_stop_to
            && raise > *floor
        {
            *floor = raise;
        }
        let (reason, use_maker) = verdict.unwrap_or_else(|| {
            if step.at_pct >= Decimal::ZERO {
                (ExitReason::TakeProfit, maker_first)
            } else {
                (ExitReason::StopLoss, false)
            }
        });
        let step_index = *done;
        *done += 1;
        tracing::info!(
            position = %pos.id,
            step = step_index,
            at_pct = %step.at_pct,
            close_ratio = %step.close_ratio,
            close_shares = %tranche,
            move_stop_to = ?step.move_stop_to,
            "exit ladder step fired"
        );
        Some(ExitRequest {
            position_id: pos.id.clone(),
            reason,
            exit_price,
            use_maker,
            close_shares: Some(tranche),
        })
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

    /// Issue #390 (replay nofast): the earliest virtual instant the exit
    /// ladder MUST run again for its verdicts to stay byte-identical to the
    /// every-tick pass — the earliest open position's expiry (the deadline
    /// rules TimeExit/ForceExit, the settlement track, and the
    /// effective-stop ladder are all functions of time-to-expiry). The
    /// throttle beats and the counter ratchets are bounded by the caller's
    /// own report beat; dirty tokens wake the pass through the caller's set.
    /// `None` with no open positions.
    pub fn next_exit_deadline_ms(&self, now_ms: i64) -> Option<i64> {
        self.open
            .iter()
            .map(|p| p.expires_at_ms)
            .filter(|t| *t > now_ms)
            .min()
    }

    /// Issue #390 (replay scheduling): the earliest clock at which the next
    /// maintenance run MUST observe something new — the earlier of the next
    /// position expiry and the next report beat (the 1 s grid that keeps the
    /// every-1 s alert stamps the every-tick pass produced). A quiet gap
    /// between this clock and the previous run re-observes the previous run's
    /// inputs, so the driver may jump the tick grid straight here without
    /// changing any output: the counter ratchets backfill over the whole gap,
    /// and nothing else in the maintenance pass reads the elapsed wall.
    /// `None` with no open positions.
    pub fn next_maintenance_deadline_ms(&self, now_ms: i64, report_beat_ms: i64) -> Option<i64> {
        self.open
            .iter()
            .map(|p| p.expires_at_ms)
            .chain(std::iter::once(now_ms.saturating_add(report_beat_ms)))
            .filter(|t| *t > now_ms)
            .min()
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
        // role of the order the caller just placed. The fee follows the schedule
        // in force, through the one expression that prices a dust remainder.
        let dust_fee = dust_fee_for(
            crate::exit_policy::fee_schedule(),
            exit_price,
            dust_notional,
            was_maker,
        );

        let cost = pos.flows.entry_cost_usd;
        let entry_fee_usd = pos.flows.entry_fee_usd;
        let exit_fee_usd = pos.flows.exit_fee_usd + dust_fee;
        // The maker rebate is CASH the venue paid for this position's maker
        // fills (modeled in replay; zero elsewhere). It enters `net` with a
        // plus sign so `balance == seed + Σ net` keeps holding: the ledger
        // credited the same amount when the fills applied.
        let maker_rebate_usd = pos.flows.maker_rebate_usd;
        let gross = exit_notional - cost;
        let net = gross - entry_fee_usd - exit_fee_usd + maker_rebate_usd;
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

        // The day's realized loss lands in the CLOSED position's account bucket
        // (per-account budgets: one account's loss never burns a sibling's day).
        self.daily_by_account
            .entry(pos.account_id.clone())
            .or_default()
            .realized_pnl_usd += net;
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
        self.note_daily_drawdown(pos.account_id.as_str(), now_ms);
        self.last_suppression_at.remove(&pos.id);
        self.unpriceable_since.remove(&pos.id);
        // #390: the ladder ledger is keyed by position id; a closed id is
        // never revisited, so drop its run stamp with the position.
        self.exit_ladder_last_run.remove(&pos.id);
        // #464: the overlay ledgers die with the position for the same reason.
        self.ladder_steps_done.remove(&pos.id);
        self.ladder_stop_floor.remove(&pos.id);

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
            maker_rebate_usd,
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
            // E28 (§9.2): the settled trade carries the position's account, so
            // per-account realized PnL is a field read.
            account_id: pos.account_id,
            // #427: the settled trade carries the position's venue.
            venue: pos.venue,
        };
        self.closed.push(closed.clone());
        if self.closed.len() > 5000 {
            let excess = self.closed.len() - 5000;
            self.closed.drain(0..excess);
        }
        Some(closed)
    }

    /// MERGE (task 2.2): burn complete UP+DOWN share-pairs of one condition into
    /// their on-chain collateral — $1.00 per pair, the collection channel of
    /// pair-discount arbitrage (@almach: 12,570 merges, zero sells).
    ///
    /// Each leg's paired shares leave at **$0.50/share** through the same
    /// `apply_exit_fill`/`close` bookkeeping every exit uses, so the two legs'
    /// exit notionals sum to exactly what [`crate::ledger::Ledger::credit_merge`]
    /// credits (`pairs × $1.00`) — the trade records and the cash ledger can
    /// never disagree. The merge itself pays NO fee: it is not a market order.
    ///
    /// Only complete pairs merge: `min(up_shares, down_shares)` floored to the
    /// share grid. A longer leg's remainder STAYS OPEN for settlement —
    /// unpaired shares have no collateral claim until the market resolves.
    /// Returns `None` (and changes nothing) unless both legs exist on the same
    /// account with at least one grid-legal pair: an on-chain merge is a
    /// single-wallet operation, and one-sided inventory has nothing to burn.
    pub fn merge_condition(&mut self, condition_id: &str, now_ms: i64) -> Option<MergeOutcome> {
        // Both legs must exist, on ONE account (a merge spends one wallet's
        // tokens on-chain; a cross-account pair is not mergeable).
        let up = self
            .open
            .iter()
            .find(|p| p.condition_id == condition_id && p.direction == SignalDirection::Up)
            .cloned()?;
        let down = self
            .open
            .iter()
            .find(|p| p.condition_id == condition_id && p.direction == SignalDirection::Down)
            .cloned()?;
        if up.account_id != down.account_id {
            tracing::warn!(
                condition = %condition_id,
                up_account = %up.account_id,
                down_account = %down.account_id,
                "merge refused: legs sit in different accounts — no single wallet owns the pair"
            );
            return None;
        }
        let pairs = floor_to_grid(up.shares).min(floor_to_grid(down.shares));
        if pairs <= Decimal::ZERO {
            return None;
        }

        // Burn the paired shares at $0.50/share per leg (fee-free, maker-role
        // bookkeeping: a merge is collateral collection, not a market exit).
        let merge_leg_price = Decimal::new(5, 1); // $0.50 per share
        let account_id = up.account_id.clone();
        // #647: the legs' own venue stamp — both legs are one account's tokens
        // on one condition, so one venue; empty when opened untagged. Read
        // before the burn moves the legs.
        let venue = up.venue.clone();
        let mut closed = Vec::new();
        for leg in [up, down] {
            // A merge is collateral collection, not a market exit: no fee and
            // no modeled rebate.
            let remaining = self.apply_exit_fill(
                &leg.id,
                pairs,
                merge_leg_price,
                Decimal::ZERO,
                OrderRole::Maker,
                Decimal::ZERO,
            );
            if remaining == Some(Decimal::ZERO) {
                // The merge consumed the whole leg: record the close. The fee
                // is zero and the exit notionals already sit in `flows`.
                if let Some(c) =
                    self.close(&leg.id, merge_leg_price, ExitReason::Merge, true, now_ms)
                {
                    closed.push(c);
                }
            }
        }
        // A merge is collateral collection, NOT a market exit: close() armed the
        // exit/asset cooldowns unconditionally, but the merged inventory is GONE
        // — @almach's rhythm is merge-then-reenter round over round, and the
        // next round's entry is a fresh decision on a fresh book, not a
        // rapid-fire re-entry into the same one. The $0.50/leg accounting makes
        // one leg's sign red by construction (entry 0.60 → exit 0.50), so a
        // per-leg "loss" is an artifact, not a signal: no loss cooldown either.
        // (A genuinely red PAIR — entry sum > 1 — is a strategy bug the entry
        // gate forbids; it never reaches this code.)
        for c in &closed {
            self.exit_cooldowns
                .remove(&cooldown_key(&c.asset, c.direction));
            self.asset_last_exit_at.remove(&c.asset);
            self.asset_last_loss_at.remove(&c.asset);
        }
        let unpaired = {
            let mut unpaired = Decimal::ZERO;
            for leg in self.open.iter() {
                if leg.condition_id == condition_id {
                    unpaired += leg.shares;
                }
            }
            unpaired
        };
        let outcome = MergeOutcome {
            condition_id: condition_id.to_string(),
            account_id,
            venue,
            pairs_merged: pairs,
            cash_credited_usd: pairs, // $1.00 per pair
            closed,
            unpaired_shares: unpaired,
        };
        Some(outcome)
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
        // The deploy line keeps the day's realized PnL inside the account's
        // `DailyLossState` (#173), so the reversal is the exact inverse of
        // `close()`'s credit — into the SAME account's bucket.
        self.daily_by_account
            .entry(closed.account_id.clone())
            .or_default()
            .realized_pnl_usd -= closed.net_pnl_usd;
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
    ///
    /// The two capacity caps count only UNEXPIRED positions: a position
    /// whose round has passed `expires_at_ms` is settlement-dead (the
    /// winner redeems, the loser pays zero — no future event can grow the
    /// exposure), yet it can be stuck on the book indefinitely when no bid
    /// ever returns (the 2026-10-11 production incident: two expired
    /// positions held both shipped slots for 6 days; a restart restores the
    /// book, so the freeze never cleared on its own). Cooldowns and the
    /// daily-loss breaker keep judging every position as before.
    ///
    /// `account` scopes BOTH capacity caps: `max_positions` and
    /// `max_positions_per_asset` are per-account books, not one shared pot —
    /// account A filling its slots must not freeze account B's fresh entry.
    /// Cooldowns stay GLOBAL: they exist to stop the strategy from re-firing
    /// into the same wound, and two accounts hold the same asset by choice.
    /// The `default` scope passes the pre-scope positions unchanged (a
    /// single-book deployment counts exactly what it counted before).
    pub fn can_open(
        &self,
        account: &str,
        asset: Option<&str>,
        direction: Option<SignalDirection>,
        now_ms: i64,
    ) -> Result<(), String> {
        let live = self
            .open
            .iter()
            .filter(|p| p.expires_at_ms > now_ms && p.account_id.as_str() == account)
            .count();
        if live >= self.config.max_positions {
            return Err(format!("Max positions ({})", self.config.max_positions));
        }
        if self.daily_loss_tripped_for(account) {
            let daily = self.daily_bucket(account);
            let limit = if daily.tripped_limit_usd > Decimal::ZERO {
                daily.tripped_limit_usd
            } else {
                self.effective_daily_loss_limit_for(account)
            };
            return Err(format!(
                "Daily loss limit for account {account} (${limit}; realized ${}, day {})",
                daily.realized_pnl_usd,
                daily.day_index.unwrap_or(0)
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
        if let Some(asset) = asset {
            // #363: the per-asset cap is a COUNT now (`max_positions_per_asset`,
            // default 1 = the historical one-per-asset behaviour), not a binary
            // "already in?" — the @Almach replay runs concurrent same-asset
            // entries across rounds, and the execution policy / CLI raises this
            // cap for strategies that model that. The refusal KEEPS the exact
            // "Already in {asset}" text: `classify_rejection` buckets it as
            // `positions.already_in` and the WebUI attribution check pins the
            // vocabulary, so a wording drift would be a silent telemetry break.
            // At the old cap of 1 the message reads exactly as before; above it,
            // the count rides the same line so an operator can see why.
            let in_asset = self
                .open
                .iter()
                .filter(|p| {
                    p.asset == asset && p.expires_at_ms > now_ms && p.account_id.as_str() == account
                })
                .count();
            if in_asset >= self.config.max_positions_per_asset as usize {
                return Err(format!("Already in {asset}"));
            }
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

#[cfg(test)]
mod tests;

/// E17 accounting contract: whatever mix of maker/taker roles and partial fills
/// a position goes through, its `net_pnl_usd` is EXACTLY the cash the ledger
/// moved for it — `(proceeds − basis) − entry_fee − exit_fee`. The full
/// end-to-end equality (`balance == seed + Σ netPnl`) is asserted on the real
/// ledger in `service::account_precision_tests`; these pin the arithmetic.
#[cfg(test)]
mod precision_tests;
