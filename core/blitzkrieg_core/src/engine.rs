//! Engine — orchestrates market data → strategies → signals → orders → exits.
//!
//! Rust port of the evaluate loop in `src/strategies/crypto-hft/index.ts`
//! (the TS tree is gone with the Node source layer, `62b16c88`).
//! The engine owns the shared market state and gates:
//!   - per-token local L2 books and per-asset Binance spot buffers
//!   - the round scanner (timing gates + token discovery)
//!   - the entry gates (one entry per token, timing, spot momentum, sizing)
//!   - near-miss telemetry for blocked entries
//!
//! Strategies are hosted behind [`crate::strategies::EngineStrategy`] (P-1.1):
//! each turns the host state into entry *candidates*, tagged with its own name;
//! the engine applies the shared gates and emits `OrderRequest`s whose
//! `strategy` field carries the emitting strategy. The kernel ships ZERO
//! strategies (PR-B): the registry starts EMPTY and every strategy — the shipped
//! examples included — enters through the C ABI v2 dlopen path
//! (`Core::load_strategy_lib`), so nothing about strategy identity is baked in.
//!
//! It is driven by `DataEvent`s (book / spot / round) and, on each evaluation,
//! asks the `Core` to place orders. Everything here is deterministic given the
//! events and an injected clock, so it is unit-testable without network.

use crate::marketdata::LocalBook;
use crate::model::{CryptoMarket, OrderbookSnapshot, Side, SignalDirection};
use crate::scanner::{RoundTiming, Scanner, ScannerConfig};
use crate::signal::{PriceBuffer, SpreadArbConfig, TradeSignal, TrendConfig};
use crate::strategies::{
    EngineStrategy, GateExemptions, HeldPosition, StrategyCtx, StrategyExitIntent,
};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

/// Compiled default for [`EngineConfig::max_orderbook_stale_ms`] (#205): the
/// value every deployment has run with since the freshness rule existed. It is
/// the DEFAULT of the runtime knob, not the knob itself — an unconfigured kernel
/// keeps this behaviour exactly.
pub const DEFAULT_MAX_ORDERBOOK_STALE_MS: i64 = 8_000;

/// Upper bound accepted for `--max-orderbook-stale-ms` (#205).
///
/// Ten minutes is already far past any usable freshness budget (the venue's
/// rounds are minutes long), so a value above it is a typo — `800000` for
/// `80000` — rather than a setting. It is refused loudly at startup instead of
/// being silently clamped: an operator who means "never treat a book as stale"
/// has `0` for that, spelled out in the usage text.
pub const MAX_ORDERBOOK_STALE_MS_CEILING: i64 = 600_000;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub scanner: ScannerConfig,
    pub trend: TrendConfig,
    pub spread_arb: SpreadArbConfig,
    /// Max orderbook staleness before we refuse to price off it (#205).
    ///
    /// The knob that decides "how long after the feed goes quiet do we stop
    /// trading", so it is a runtime setting, not a compiled one: it comes from
    /// `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS` through
    /// [`crate::service::CoreConfig::max_orderbook_stale_ms`]. `0` = the check is
    /// OFF (any age is accepted); the compiled default is
    /// [`DEFAULT_MAX_ORDERBOOK_STALE_MS`], so an unconfigured kernel behaves
    /// exactly as it did before the knob existed.
    pub max_orderbook_stale_ms: i64,
    /// Spot momentum window (sec) used by the alignment filter.
    pub momentum_window_sec: i64,
    /// Tolerance for spot moving against the entry direction (%).
    pub momentum_tol_pct: Decimal,
    /// Internal-key base so the OME dedups repeats within a round.
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// P0 #202 — the per-entry budget as a PERCENTAGE of the account's cash
    /// equity, instead of the absolute `size_usd`. 0 = OFF (the shipped
    /// default): the absolute path below runs unchanged, byte for byte.
    ///
    /// An absolute budget is not a risk statement: `size_usd = 2.5` with the
    /// default share band [10, 10] sends 10 shares whatever the price, which on
    /// the live 4.8 USDC account is 4.00 USD — 83% of it — and the band, not
    /// the budget, is what decided that. A percentage is the same statement on
    /// every account size: 20% is 0.96 USD on a 4.8 book and 96 on a 480 one.
    ///
    /// When > 0 this REPLACES `size_usd` (an absolute budget left in place
    /// would cap a grown account at the old size, which is the failure this
    /// knob exists to remove). See `compute_shares` for the exact rules.
    pub size_pct: Decimal,
    /// Per-strategy sizing overrides (E2-a). Absent strategies use the globals;
    /// a present override is always clamped so it can never exceed the global
    /// risk values — global is both the fallback and the ceiling.
    pub strategy_sizes: HashMap<String, StrategySize>,
}

/// Optional per-strategy sizing knobs (E2-a). Every field is optional; `None`
/// falls back to the matching global `EngineConfig` value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrategySize {
    pub size_usd: Option<Decimal>,
    pub min_shares: Option<Decimal>,
    pub max_shares: Option<Decimal>,
    /// Relative allocation weight (E16/#98): scales this leg's per-entry
    /// notional (own override or global × weight). A weight can only shrink
    /// the budget — anything above 1 is clamped back to the global cap.
    pub size_weight: Option<Decimal>,
    /// This leg's own equity percentage (#202). `None` = the global
    /// `size_pct`. Clamped to the global when the global is armed (> 0), and
    /// free when the global is off — a per-strategy percentage is then the
    /// only one in force, never a zero.
    pub size_pct: Option<Decimal>,
}

impl StrategySize {
    /// True when at least one sizing dimension overrides the global values (a
    /// `StrategyLimit` that configures only caps reports false).
    pub fn overrides_anything(&self) -> bool {
        self.size_usd.is_some()
            || self.min_shares.is_some()
            || self.max_shares.is_some()
            || self.size_weight.is_some()
            || self.size_pct.is_some()
    }
}

/// The sizing knobs actually in force for one strategy after clamping.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveSizing {
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// The equity percentage in force for this leg (#202); 0 = absolute path.
    pub size_pct: Decimal,
    /// Whether a per-strategy override set any of these values.
    pub strategy_scoped: bool,
    /// The weight in force (`None` = unweighted).
    pub size_weight: Option<Decimal>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            scanner: ScannerConfig::default(),
            trend: TrendConfig::default(),
            spread_arb: SpreadArbConfig::default(),
            max_orderbook_stale_ms: DEFAULT_MAX_ORDERBOOK_STALE_MS,
            momentum_window_sec: 30,
            momentum_tol_pct: Decimal::new(3, 2), // 0.03%
            size_usd: Decimal::new(25, 1),        // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
            // Off by default: the absolute path is what every deployment runs
            // today, and arming this would change their order sizes silently.
            size_pct: Decimal::ZERO,
            strategy_sizes: HashMap::new(),
        }
    }
}

/// Per-strategy gate bookkeeping (E2-b / #27): how often a strategy's own
/// candidates were stopped by a gate, and how often a declared exemption let one
/// through anyway. Both halves are reported per strategy so an opt-out is
/// never invisible next to an un-exempted strategy's rejection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrategyGateTally {
    pub blocked_timing: u64,
    pub blocked_momentum: u64,
    pub exempted_timing: u64,
    pub exempted_momentum: u64,
}

/// Why a valid signal did not become an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// The round timing gate (min age / min time left) rejected it.
    Timing,
    /// The spot momentum filter rejected it.
    Momentum,
}

/// A signal the evaluator produced that a gate blocked (near-miss telemetry).
#[derive(Debug, Clone)]
pub struct BlockedCandidate {
    /// The strategy whose candidate was blocked — so `blocked.timing` /
    /// `blocked.momentum` are attributable per strategy (E2-b / #27).
    pub strategy: String,
    /// The account scope the candidate came from (the signal's own stamp),
    /// so the gate counts can be sliced per account like every other
    /// strategy counter (PR H / 账户跟随).
    pub account_id: String,
    pub token_id: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub price: Decimal,
    pub reason: BlockReason,
    pub time_left_sec: i64,
}

/// One honoured per-strategy gate exemption (E2-b / #27): the strategy declared
/// it did not need a shared entry gate and the host let its candidate through.
/// Recorded so an opt-out is always auditable — the log line is the Chinese
/// sentence «本单因策略 X 豁免门禁 Y», and the record carries the numeric detail
/// of what the gate would have said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateExemptionRecord {
    pub strategy: String,
    /// The account scope the exempted candidate came from (the signal's own
    /// stamp) — the audit trail's account dimension, same as the block trail.
    pub account_id: String,
    /// "timing" | "momentum".
    pub gate: &'static str,
    pub token_id: String,
    pub asset: String,
    /// The block the gate would have produced (e.g. `"Too close to expiry
    /// (120s < 180s)"` or `"spot BTC -1.20% in 30s vs Up (tol 0.03%)"`).
    pub detail: String,
    pub time_left_sec: i64,
}

impl GateExemptionRecord {
    /// The auditable one-line sentence for logs/diagnostics.
    pub fn audit_line(&self) -> String {
        format!(
            "本单因策略 {} 豁免门禁 {}（{}，token={}）",
            self.strategy, self.gate, self.detail, self.token_id
        )
    }
}

/// The kind of a wallet-level cash flow (#388). The activity ledger pays them
/// as whole-ledger rows — never per fill — so they ride the stream as they
/// print and the ledger credits each row exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CashflowKind {
    /// MAKER_REBATE: the venue's maker-side rebate, paid per ledger row.
    Rebate,
    /// REWARD: a venue reward program payout (e.g. liquidity rewards).
    Reward,
}

impl CashflowKind {
    /// The archive's `"k":"cashflow"` `which` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CashflowKind::Rebate => "rebate",
            CashflowKind::Reward => "reward",
        }
    }

    /// Inverse of [`CashflowKind::as_str`]; `None` is a fail-closed parse
    /// error, never a silent default.
    pub fn parse_kind(s: &str) -> Option<Self> {
        match s {
            "rebate" => Some(CashflowKind::Rebate),
            "reward" => Some(CashflowKind::Reward),
            _ => None,
        }
    }
}

/// One book of a batch: `(token_id, bids, asks)` — the shape
/// [`DataEvent::Books`] and [`Engine::on_data_books`] both carry, named so the
/// signatures stay readable.
pub type BatchBook = (String, Vec<(Decimal, Decimal)>, Vec<(Decimal, Decimal)>);

/// Market-data events the engine consumes (produced by the feed layer or, in
/// tests, driven directly).
#[derive(Debug, Clone)]
pub enum DataEvent {
    /// Full or incremental orderbook update for a token.
    Book {
        token_id: String,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    },
    /// 盘口批次原子化：一次 REST 批式拉取（如 polymarket `POST /books`）的
    /// 整批 book，`now_ms` 是这个批次的统一时间戳。整批必须一次应用 ——
    /// 逐 token 发射会在两次锁的间隙让策略读到「一半新一半旧」的撕裂状态
    /// （up 腿是本批、down 腿还是上一批，一对不存在的折价就是从这来的）。
    /// 批内按 `Vec` 顺序应用；每一本仍走与 [`DataEvent::Book`] 完全相同的
    /// apply/notify/kline 路径，只是共享一次锁与一个批次时间戳。
    Books { books: Vec<BatchBook>, now_ms: i64 },
    /// Top-of-book only update (price_change / best_bid_ask).
    TopOfBook {
        token_id: String,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
        now_ms: i64,
    },
    /// Binance spot price for an asset (e.g. "BTC").
    Spot {
        asset: String,
        price: Decimal,
        now_ms: i64,
    },
    /// Fresh round markets discovered by the scanner.
    RoundMarkets {
        markets: Vec<CryptoMarket>,
        now_ms: i64,
    },
    /// #352: one on-chain print observed on a token — information only.
    /// The engine does not trade on it; replay consumers (e.g. an @almach
    /// benchmark) and the archive completeness counters do.
    Trade {
        token_id: String,
        side: Side,
        price: Decimal,
        size: Decimal,
        now_ms: i64,
    },
    /// #352: a round's markets resolved — the boundary event a replay uses to
    /// close the book on that round. Information only.
    RoundEnd { now_ms: i64 },
    /// #377 (治 #2): one market's on-chain verdict, recovered by the converter
    /// from the wallet's own redemption activity and replayed as a first-class
    /// event. The Core settles from it directly (`on_market_resolution`) —
    /// this is what closes the dry ladder's blind spot, where book evidence
    /// below the coin-flip withheld every settlement and stuck the legs. The
    /// engine itself trades on books and spot only; for it this is a marker.
    Resolution {
        condition_id: String,
        /// `(token_id, payout per share)` in the market's own outcome order —
        /// a binary boundary names BOTH sides explicitly: the winner pays 1,
        /// the loser 0. The core books what the stream says and nothing else.
        payouts: Vec<(String, Decimal)>,
        /// The market settles through the NegRisk adapter.
        neg_risk: bool,
        now_ms: i64,
    },
    /// #388: one wallet-level cash row from the activity ledger
    /// (MAKER_REBATE / REWARD) — money the venue paid the wallet OUTSIDE any
    /// fill, priced in whole-ledger rows. The core credits the active
    /// account's cash at the row's own timestamp; no order, position or
    /// settlement semantics touch it, so a corpus without these rows replays
    /// byte-identically to before.
    Cashflow {
        which: CashflowKind,
        /// The row's `usdcSize` — cash paid to the wallet, USD.
        amount_usd: Decimal,
        now_ms: i64,
    },
    /// #427: an operator-confirmed cross-venue mapping (`k:"unified"`), the
    /// #425 `UnifiedEvent` carried on the event stream. Pure state for the
    /// strategy layer: it tells a strategy WHICH tokens across venues are the
    /// same real-world bet, so a cross-venue view can be assembled from the
    /// venue-tagged books already flowing. The engine does not trade on it
    /// (no book update — same treatment as `Trade`/`Resolution`); a corpus
    /// without these rows replays byte-identically to before.
    UnifiedMapping {
        event: blitzkrieg_market_api::unified::UnifiedEvent,
        now_ms: i64,
    },
}

/// A strategy registered with the engine, plus its enablement and provenance.
pub struct HostedStrategy {
    pub strategy: Box<dyn EngineStrategy>,
    pub enabled: bool,
    /// "builtin" or "dylib:<path>" / "test".
    pub source: String,
    /// The account scope this instance trades for. The default-scope instance
    /// (`default_account_id()`) is the compatibility shape: a deployment that
    /// never assigns strategies per account runs exactly one instance per
    /// strategy, targeted at `default`, and every byte of the replay baseline
    /// is unchanged. Additional scopes are per-account instances the host
    /// installs from the account's strategy ENABLEMENT set (the panel's
    /// per-(account, strategy) 启用按钮; strategies are global, this records
    /// which account×strategy pairs run).
    pub account_id: String,
    /// EXITS-ONLY demotion (the stop-safety net): a strategy whose scope was
    /// DISABLED (or removed from an account's enablement set) while it still
    /// holds OPEN positions for that account stays hosted but contributes no
    /// ENTRY candidates — its exit intents keep flowing until the account is
    /// flat, then the next enablement write (or boot) drops the scope.
    /// `false` on every normally-registered instance.
    pub exits_only: bool,
}

/// The scope-instantiation factory the host passes to
/// `rebuild_account_scopes`: given (strategy name, account), build a fresh
/// instance for that account — or `None` when the source is not
/// rebuildable (the name is skipped with a warning, never a panic).
pub type ScopeBuilder<'a> = dyn FnMut(&str, &str) -> Option<Box<dyn EngineStrategy>> + 'a;

pub struct Engine {
    cfg: EngineConfig,
    scanner: Scanner,
    books: HashMap<String, LocalBook>,
    spot: HashMap<String, PriceBuffer>,
    /// Tokens with a live entry order this round, to avoid re-signalling.
    /// Keyed `(account_id, token_id)`: two accounts trading the same token
    /// are two independent entries, and neither may suppress the other.
    /// The default-scope deployment keeps exactly the old behaviour — its
    /// keys are `("default", token)`, indistinguishable in effect from the
    /// old bare-token set.
    pending_tokens: HashSet<(String, String)>,
    /// Issue #390 (replay nofast): the candidate signals of the most recent
    /// REAL evaluation — the set the quiet-tick replay cycles re-judge. The
    /// inputs behind a candidate (books, spot, trend tracker, round list) are
    /// event-driven, so between events the set cannot change; a quiet tick
    /// re-runs it through the kernel-side gates instead of paying a Lua call.
    /// Cleared on the round rollover and re-staged by every real evaluation.
    /// Exit intents are deliberately NOT staged: their first delivery after a
    /// real evaluation is consumed on the very next tick (forced onto it by
    /// `has_forced_next_tick`), and a re-delivery is unobservable — a merge
    /// intent against already-merged conditions resolves to nothing.
    last_candidates: Vec<TradeSignal>,
    /// Issue #390 (replay nofast): whether the standing candidate set is in
    /// a STABLE phase — the last two real evaluations produced the SAME
    /// candidates. A strategy whose output changes between cycles (a latch:
    /// pair_discount_arb arms on emission and never re-emits the attempt) is
    /// mid-phase-transition, and re-playing the previous output there would
    /// retry an entry the base walk's next collection would never produce.
    /// A stable set (spread_arb re-emits its confirmed-token candidates every
    /// cycle) is exactly the state a quiet-tick replay reproduces. Unstable
    /// phases fall back to real evaluations until the output settles.
    candidates_stable: bool,
    /// E25 (#331): `time_left_sec` of the most recent evaluate cycle — the
    /// arbitration physics projection reads it (`effective_stop_pct`).
    last_time_left_sec: i64,
    /// Near-miss signals from the most recent evaluate().
    last_blocked: Vec<BlockedCandidate>,
    /// Per-strategy gate exemptions HONOURED during the most recent evaluate()
    /// (E2-b / #27) — the auditable "本单因策略 X 豁免门禁 Y" trail.
    last_exemptions: Vec<GateExemptionRecord>,
    /// Records blocked signals + their subsequent price path (for offline
    /// evaluation of relaxing the entry gate). Observation-only.
    near_miss: crate::shadow::NearMissRecorder,
    blocked_timing_total: u64,
    blocked_momentum_total: u64,
    /// Session per-strategy gate counts (blocked vs exempted), E2-b / #27.
    gate_tally: HashMap<String, StrategyGateTally>,
    /// Session per-(strategy, account) gate counts — the ACCOUNT slice of
    /// [`Engine::gate_tally`], folded alongside it in `tally_blocked` /
    /// `take_exemptions`. The key is `format!("{strategy}\u{1}{account}")`,
    /// the same shape service.rs's `strategy_acc_key` builds (the engine does
    /// not depend on the service; the separator cannot appear in a strategy
    /// name, so the pair is unambiguous).
    gate_tally_acc: HashMap<String, StrategyGateTally>,
    /// Optional per-strategy hot-parameter registry for Shadow Evolution
    /// (E2-c / #28). When present, each strategy resolves its OWN cell through it
    /// and reads that cell lock-free on the hot path, so an evolution takes effect
    /// on the next tick with no restart — and one strategy's parameters can never
    /// be read by another.
    hot_params: Option<std::sync::Arc<crate::shadow_evolution::ParamRegistry>>,
    /// Registered strategies (PR-B: starts EMPTY — the kernel ships none). Every
    /// strategy arrives through `register_user_strategy`, whether it was built
    /// in-tree in a test or dlopened from a cdylib.
    strategies: Vec<HostedStrategy>,
    /// Provenance of every package ever LOADED this session (name → the
    /// source string its first registration recorded). Outlives the scopes:
    /// an explicit assignment that drops the last scope of a strategy must
    /// not erase the ability to re-assign it — the assignable set and the
    /// scope re-builder both read this, not the live scope list.
    package_sources: std::collections::BTreeMap<String, String>,
    /// Strategy close intents gathered during the latest evaluate(), drained by
    /// the host (`Core`) into its shared exit-submission path.
    strategy_exits: Vec<StrategyExitIntent>,
    /// THIS cycle's open positions, pushed by the host before each evaluation
    /// (the pair gap-fill surface, the same shape as `set_equity_usd`'s host
    /// push). One flat list spanning every strategy: `evaluate` slices it per
    /// strategy before the ctx is built, so a strategy can only ever see its
    /// own book. Empty until the host pushes — every diagnostic path keeps
    /// today's shape.
    positions_view: Vec<HeldPosition>,
    /// RFC 568 阶段一：本周期的配对簿视图快照（`set_pair_views_snapshot`
    /// 的 host push，形状同 `positions_view`）。一条平铺所有策略的列表：
    /// `evaluate` 按 owner 策略切片进各自的 ctx，跨策略/跨账户不可见已由
    /// Core 侧 `views_for` 的 owner 过滤做完。Empty until the host pushes
    /// —— 诊断路径与影子孪生保持旧形状（读不到按「无登记」处理）。
    pair_views_snapshot: Vec<crate::pair::PairView>,
    /// The account's cash equity, pushed by the host before each evaluation
    /// (#202). ZERO = not (yet) supplied. Only the equity-relative sizing reads
    /// it: with `size_pct = 0` the engine's tickets do not depend on this at
    /// all, so an unconfigured deployment cannot be affected by how (or
    /// whether) the host feeds it.
    ///
    /// Default-scope sizing reads this scalar. Non-default account scopes
    /// read `equity_by_account` instead — the host pushes EVERY trading
    /// account's book each cycle, so a scope's ticket sizes against its own
    /// account's cash, never the process-active ledger.
    equity_usd: Decimal,
    /// Per-account equity pushed by the host before each evaluation, for
    /// non-default scopes. Absent = the host has not pushed that account
    /// (sizing then treats the budget as zero — fail-closed, no ticket).
    equity_by_account: HashMap<String, Decimal>,
    /// Per-account trading switches mirrored from the account book
    /// (`account.trading.set`). An OFF scope still runs its exits (open
    /// positions stay managed to settlement) but contributes NO entry
    /// candidates — `submit_candidate_orders` drops them before any gate.
    /// Default scope is always ON: the switch is an account-book fact and
    /// `default` predates it.
    trading_by_account: HashMap<String, bool>,
    /// Per-account freeze mirrored from the service's kill book
    /// (`freeze_account`/`resume_account`). A frozen scope drops its entry
    /// candidates exactly like a trading-OFF one; exits flow. Unknown
    /// account = not frozen (the service state is the truth; this is the
    /// read-side mirror).
    frozen_by_account: HashMap<String, bool>,
    /// Per-account venue binding mirrored from the account book
    /// (`account.venue.set`). Empty string = unbound. Candidates emitted by
    /// a scope whose venue cannot trade the candidate's token are dropped
    /// IN THE ENGINE — cheaper than a place-time refusal and it keeps the
    /// rejection-cooldown book free of entries that were never tradable.
    venue_by_account: HashMap<String, String>,
    /// Static mirror relationships between venues: `mirror_of[mirror] =
    /// source` — the mirror venue trades its source's tokens (byte-identical
    /// ids) through its own executor while the source's feed owns the table
    /// rows. PolySimulator is a CONSTRUCTIVE mirror of Polymarket (its token
    /// ids ARE Polymarket's by construction), so its admission/routing
    /// evidence no longer depends on its own round declarations surviving a
    /// merge (the #634 shadow batch — which dies when the parasitic plugin
    /// stops declaring rounds). Written by the host from the plugin's
    /// declared mirror relationship; absent = not a mirror.
    mirror_of: HashMap<String, String>,
    /// Signals the equity-relative sizing produced NO ticket for (#202) —
    /// a budget that cannot buy one whole share. Counted rather than silent:
    /// "why is nothing trading?" must be answerable from the stats snapshot.
    size_pct_skipped: u64,
    /// E29 (§10.3): the K-line aggregator. Fed from the SAME data path that
    /// already reaches `on_data` (book mids — the venue publishes no trade
    /// tape this deployment can read; §10.3's "input source" is the existing
    /// data path, whatever it carries). Closed bars dispatch `on_kline` and
    /// are drained by the host as `KLINE_UPDATE` events.
    klines: crate::kline::KlineAggregator,
    /// #427: the latest confirmed cross-venue mapping per event id, keyed by
    /// event id. Pure strategy-layer state — the engine never trades on it,
    /// the read view (`unified_mappings()`) exposes it to the strategy host
    /// so a cross-venue package can find both legs' tokens. Last write wins
    /// (a re-confirmation overwrites; a revocation arrives as a mapping with
    /// one listing).
    unified_mappings:
        std::collections::BTreeMap<String, blitzkrieg_market_api::unified::UnifiedEvent>,
    /// 跨所套利弹窗 v2: the arb leg configuration in force (the host's hot
    /// push, mirroring accounts.toml's `[cross_venue_arb]`). `None` =
    /// unconfigured — every stamp falls back to the instance account and no
    /// arb params are merged. Read by `evaluate`'s strategy loop (each
    /// adapter re-resolves the leg map per candidate) and by the params
    /// merge; written by `set_arb_leg_config`.
    arb_leg_config: Option<crate::account::CrossVenueArbConfig>,
    /// RFC 568 阶段一：带 pair 声明且过了 scope 门（含 A≠B 账户校验）的
    /// 腿声明 —— Core 在 arb_precheck 过闸后消费（declare 落 PairBook）。
    /// 每次 evaluate 前清空；replay 静默周期不产出（staged 集合重放的是
    /// 无 pair 上下文的 OrderRequest —— 声明是真实评估的伴生事实）。
    pair_declarations: Vec<PairDeclaration>,
    /// RFC 568 阶段一：A≠B 校验的拒绝回执（补钉 5 权威位 —— 同 pair 两腿
    /// effective account_id 相同 ⇒ 拒绝声明）。Core 转 APPLICATION 错误
    /// 事件（点名字段）；被拒声明对应的腿声明不产出（订单路径照旧）。
    pair_refusals: Vec<String>,
    /// 镜像盘显示簿（2026-10-25 poly 轮钟冻结定谳）：PolySimulator 镜像
    /// Polymarket —— 同窗口的 condition/token id 逐字节相同（生产档案
    /// 证实）。scanner 表按 token 唯一键行走（`market_for_token`/
    /// `venue_of` 都是一查即中），两家的行不能并存 —— 后到的那家批次在
    /// merge 臂被判「全是已知 token」吞掉，它的 (venue, duration) 轮钟
    /// 与行情行双双落不了地，面板切到该所账户永远「加载中」。
    ///
    /// 处置：交易面保持单行（镜像即同一市场，表里一份就够）；显示面把
    /// 被遮蔽 venue 自己声明的批次存进这张簿 —— 键 (venue, duration)，
    /// 值该所最近一次申报的整批行。只有行情装配层（ENGINE_ROUND）读它，
    /// 给「钟已开口但表里没有自己的行」的 venue 合成带它标签的行情行
    /// （token 相同 → 共享同一份 book，价格是真实读数）。交易面零接触：
    /// 入场门、token→市场查询、结算对侧证据都不查这张簿（#643 收口：
    /// #634 短暂把 shadow 批次接进 scope 放行与执行路由，已撤——镜像
    /// 归属以 `mirror_of` 自声明为唯一交易真值）。
    shadow_markets: HashMap<(String, i64), Vec<CryptoMarket>>,
}

/// RFC 568 阶段一：一条过了 scope 门的配对声明（engine → Core 的提交伴生
/// 事实）。owner_account 是实例主账户（PairBook 的实例隔离维度）；腿的
/// 落账账户（stamp 结果）单独携带，A≠B 校验在 engine 侧判它。
#[derive(Debug, Clone)]
pub struct PairDeclaration {
    pub owner_strategy: String,
    pub owner_account: String,
    pub leg_account: String,
    pub decl: crate::signal::TradePairDecl,
    pub token_id: String,
    /// 策略侧声明的股数（None = 内核 sizing —— declare 记 0，PairBook 的
    /// 股数字段只作读数，不参与防重/状态机）。
    pub shares: Option<Decimal>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        // ZERO strategies: the kernel has no builtins (PR-B). Anything that
        // trades is registered by the host through the C ABI v2 loader.
        let strategies = Vec::new();
        Self {
            scanner: Scanner::new(cfg.scanner.clone()),
            books: HashMap::new(),
            spot: HashMap::new(),
            pending_tokens: HashSet::new(),
            last_candidates: Vec::new(),
            candidates_stable: false,
            last_time_left_sec: 0,
            last_blocked: Vec::new(),
            last_exemptions: Vec::new(),
            near_miss: crate::shadow::NearMissRecorder::new(500, 300_000),
            blocked_timing_total: 0,
            blocked_momentum_total: 0,
            gate_tally: HashMap::new(),
            gate_tally_acc: HashMap::new(),
            hot_params: None,
            strategies,
            package_sources: std::collections::BTreeMap::new(),
            strategy_exits: Vec::new(),
            positions_view: Vec::new(),
            pair_views_snapshot: Vec::new(),
            cfg,
            equity_usd: Decimal::ZERO,
            equity_by_account: HashMap::new(),
            trading_by_account: HashMap::new(),
            frozen_by_account: HashMap::new(),
            venue_by_account: HashMap::new(),
            mirror_of: HashMap::new(),
            size_pct_skipped: 0,
            klines: crate::kline::KlineAggregator::with_default_intervals(),
            unified_mappings: std::collections::BTreeMap::new(),
            arb_leg_config: None,
            pair_declarations: Vec::new(),
            pair_refusals: Vec::new(),
            shadow_markets: HashMap::new(),
        }
    }

    /// RFC 568 阶段一：排空本周期收集的配对声明（Core 在 arb_precheck 过闸
    /// 后消费；每次真实评估前由 evaluate 清空）。
    pub fn take_pair_declarations(&mut self) -> Vec<PairDeclaration> {
        std::mem::take(&mut self.pair_declarations)
    }

    /// RFC 568 阶段一：排空本周期 A≠B 校验的拒绝回执（补钉 5；Core 转
    /// APPLICATION 错误事件）。
    pub fn take_pair_refusals(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pair_refusals)
    }

    /// 跨所套利弹窗 v2: hot-push the arb leg configuration (the host calls it
    /// on `arb.config.set`/`clear` and at registration). Fanned to every
    /// hosted strategy's adapter — the stamp source AND the params bag update
    /// in the same call, so a set is fully in force on the next evaluation.
    ///
    /// The fan-out is a PER-SCOPE PROJECTION, not a verbatim broadcast: a
    /// scope whose instance account IS one of the configured leg accounts
    /// receives the map as the user spelled it — its legs stamp per venue.
    /// Any other scope of the same package is a SEPARATE deployment (a
    /// second account running the package independently, the account-decouple
    /// posture): it keeps its own book whole — its projection rewrites BOTH
    /// leg accounts onto its own instance id, so its pair fills under its
    /// scope account exactly as the pre-v2 shape did, and it can never
    /// double-place legs onto the first deployment's accounts. Unconfigured
    /// (= None) fans out as None everywhere.
    pub fn set_arb_leg_config(&mut self, cfg: Option<crate::account::CrossVenueArbConfig>) {
        self.arb_leg_config = cfg.clone();
        for s in &mut self.strategies {
            let scope_cfg = match (&cfg, s.account_id.as_str()) {
                (Some(c), acct)
                    if acct == c.leg_a_account.trim() || acct == c.leg_b_account.trim() =>
                {
                    Some(c.clone())
                }
                (Some(c), acct) => {
                    let mut projected = c.clone();
                    projected.leg_a_account = acct.to_string();
                    projected.leg_b_account = acct.to_string();
                    Some(projected)
                }
                (None, _) => None,
            };
            s.strategy.set_arb_leg_config(scope_cfg);
        }
    }

    /// The arb leg configuration in force (diagnostics/tests read the engine
    /// mirror; the Core cell remains the wire truth).
    pub fn arb_leg_config(&self) -> Option<&crate::account::CrossVenueArbConfig> {
        self.arb_leg_config.as_ref()
    }

    /// #427: record one confirmed cross-venue mapping (last write wins per
    /// event id). Called by the host when a `UnifiedMapping` event arrives.
    pub fn put_unified_mapping(&mut self, event: blitzkrieg_market_api::unified::UnifiedEvent) {
        self.unified_mappings.insert(event.id.clone(), event);
    }

    /// ④: drop one confirmed mapping (operator revocation). The Engine holds
    /// only `put` semantics — the host removes the copy here on
    /// `unified.revoke` so strategies stop seeing the event immediately.
    /// Removing an unknown id is a no-op.
    pub fn remove_unified_mapping(&mut self, event_id: &str) {
        self.unified_mappings.remove(event_id);
    }

    /// #427: the confirmed cross-venue mappings, event-id ordered. The read
    /// view behind `bk.unified_events()` — an immutable snapshot the strategy
    /// layer assembles its cross-venue legs from.
    pub fn unified_mappings(
        &self,
    ) -> &std::collections::BTreeMap<String, blitzkrieg_market_api::unified::UnifiedEvent> {
        &self.unified_mappings
    }

    /// The account equity the equity-relative sizing (#202) is a percentage of
    /// — the host pushes it from the ledger before each evaluation, so the
    /// ticket follows the account it is actually trading. A non-positive value
    /// means "unknown": a leg with `size_pct > 0` then emits nothing, because
    /// sizing against an unknown account has no safe reading, and guessing the
    /// absolute budget back in would silently restore the fixed lot #202 is
    /// about.
    pub fn set_equity_usd(&mut self, equity: Decimal) {
        self.equity_usd = equity;
    }

    /// Push one account scope's book. The host calls this for EVERY account
    /// with a strategy assignment before each evaluation (the default account
    /// included — its value mirrors `set_equity_usd`). A scope without a push
    /// sizes against zero: fail-closed, no ticket.
    pub fn set_account_equity(&mut self, account: &str, equity: Decimal) {
        self.equity_by_account.insert(account.to_string(), equity);
    }

    /// Mirror one account's trading switch (account book fact). OFF = the
    /// scope contributes no entry candidates; its exits keep flowing.
    pub fn set_account_trading(&mut self, account: &str, enabled: bool) {
        self.trading_by_account.insert(account.to_string(), enabled);
    }

    /// Mirror one account's freeze (account-scoped kill switch). A frozen
    /// scope behaves exactly like a trading-OFF one — entries dropped,
    /// exits flowing — so a venue outage that freezes account A no longer
    /// stops account B from trading. Service-owned fact, engine only reads.
    pub fn set_account_frozen(&mut self, account: &str, frozen: bool) {
        self.frozen_by_account.insert(account.to_string(), frozen);
    }

    /// Mirror one account's venue binding (`account.venue.set`). Empty string
    /// = unbound (may trade any venue's tokens).
    pub fn set_account_venue(&mut self, account: &str, venue: &str) {
        self.venue_by_account
            .insert(account.to_string(), venue.to_string());
    }

    /// Declare a static mirror relationship: `mirror` trades its `source`'s
    /// tokens (byte-identical ids) through its own executor while the source's
    /// feed owns the table rows. THE admission/routing evidence: scope
    /// admission (`scope_admits_token`) and execution routing
    /// (`execution_venue_for`) read this declaration and nothing else — it
    /// survives the mirror venue going quiet (the parasitic PolySimulator
    /// never declares rounds), where runtime observation would wobble with
    /// the feed (#643 retired the shadow-batch fallback).
    pub fn set_venue_mirror(&mut self, mirror: &str, source: &str) {
        if mirror.is_empty() || source.is_empty() || mirror == source {
            self.mirror_of.remove(mirror);
            return;
        }
        self.mirror_of
            .insert(mirror.to_string(), source.to_string());
    }

    /// The venue `mirror` statically mirrors (`None` when it declares none).
    pub(crate) fn mirror_source_of<'a>(&'a self, mirror: &str) -> Option<&'a str> {
        self.mirror_of.get(mirror).map(|s| s.as_str())
    }

    /// Every static mirror declaration as (mirror, source) pairs — the
    /// display plane's synthesis loop iterates these (a parasitic mirror
    /// venue has NO observed clock of its own to loop over).
    pub(crate) fn mirror_pairs(&self) -> Vec<(String, String)> {
        self.mirror_of
            .iter()
            .map(|(m, s)| (m.clone(), s.clone()))
            .collect()
    }

    /// The equity a scope's sizing reads: the account's own pushed book for
    /// non-default scopes, the legacy scalar for the default scope.
    fn scope_equity(&self, account: &str) -> Decimal {
        if account == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
            self.equity_usd
        } else {
            self.equity_by_account
                .get(account)
                .copied()
                .unwrap_or(Decimal::ZERO)
        }
    }

    /// Whether a scope may emit entry candidates this cycle. The default
    /// scope has no account-book switch (always true); every other scope
    /// reads the mirrored `trading_by_account` fact — an unknown account
    /// counts as OFF (fail-closed).
    fn scope_trading(&self, account: &str) -> bool {
        if account == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
            return true;
        }
        self.trading_by_account
            .get(account)
            .copied()
            .unwrap_or(false)
    }

    /// Whether a scope is account-frozen (account-scoped kill switch). The
    /// default scope reads the same map: a manual `account.freeze default`
    /// must stop it too, so unlike `scope_trading` there is no privileged
    /// default arm. Unknown account = not frozen.
    fn scope_frozen(&self, account: &str) -> bool {
        self.frozen_by_account
            .get(account)
            .copied()
            .unwrap_or(false)
    }

    /// Whether THIS (strategy, account) instance is demoted exits-only. The
    /// demotion is per instance: the default scope is never demoted (it is
    /// not assignment-managed), and a non-default instance only when a
    /// rebuild left it hosted for its held positions.
    fn scope_exits_only(&self, strategy: &str, account: &str) -> bool {
        if account == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
            return false;
        }
        self.strategies
            .iter()
            .any(|s| s.account_id == account && s.strategy.name() == strategy && s.exits_only)
    }

    /// Whether a scope's venue binding admits a token: unbound admits
    /// everything; bound scopes only their own venue's tokens (an unknown
    /// token venue = not admitted — the token must belong to a market this
    /// deployment trades). A market with NO venue tag (`m.venue` empty —
    /// corpora from the single-plugin world, where the venue was implicit)
    /// admits every bound scope: the archive predates #427's tagging, and
    /// there is nothing to disagree with the binding about — refusing those
    /// made any venue-bound account go silent on every pre-tag corpus (the
    /// exit-economics fixture first tripped it: signals 89975→0, closes 13→0).
    ///
    /// 镜像盘（PolySimulator 镜像 Polymarket，token 逐字节相同）：交易表按
    /// token 唯一存行，先到的 PM 批占住行，镜像 token 的表行永远标着被镜像
    /// 所。绑镜像所的账户查表必得「别所的行」，每条候选静默出局（真线：
    /// signals=0 / blocked=0，连遥测都不留）。mirror admission 只读插件的
    /// 自声明（`mirror_of`，#638 的构造性事实，boot/绑定时推入）：绑定所
    /// 声明镜像表行所属的所即放行。声明是交易面的唯一镜像证据 —— 显示簿
    /// （`shadow_markets`）不是：运行时观测随 feed 时效摆动（feed 断则放
    /// 行消失），声明不会（#643 把 shadow 批次退出交易裁决）。
    /// crate-visible so the IPC runtime-enablement tests can pin the wire
    /// behaviour the admission gate actually answers.
    pub(crate) fn scope_admits_token(&self, account: &str, token: &str) -> bool {
        let Some(bound) = self.venue_by_account.get(account).filter(|v| !v.is_empty()) else {
            return true;
        };
        match self.scanner.market_for_token(token) {
            None => false,
            Some(m) => {
                m.venue.is_empty()
                    || m.venue == *bound
                    || self.mirror_source_of(bound) == Some(m.venue.as_str())
            }
        }
    }

    /// The open-position snapshot the NEXT evaluation's ctx carries (the pair
    /// gap-fill surface). The host pushes the full list — every strategy, the
    /// shared PositionManager's current book — and `evaluate` slices it per
    /// strategy, so one strategy never sees another's positions. Stale between
    /// pushes by design: a cycle's view is the book as of its own start, the
    /// same consistency the books and equity in that ctx have.
    pub fn set_positions_view(&mut self, positions: Vec<HeldPosition>) {
        self.positions_view = positions;
    }

    /// RFC 568 阶段一：push this cycle's pair-book views（`bk.arb.pairs()`
    /// 的数据源）。The host pushes the full list — Core 已按策略×账户白名单
    /// 过滤——`evaluate` 再按 owner 策略切片进各自的 ctx。Stale between
    /// pushes by design（最终一致，附注 2）：a cycle's view is the book as
    /// of its own start.
    pub fn set_pair_views_snapshot(&mut self, views: Vec<crate::pair::PairView>) {
        self.pair_views_snapshot = views;
    }

    pub fn equity_usd(&self) -> Decimal {
        self.equity_usd
    }

    /// Signals skipped because an equity-relative budget could not buy one
    /// whole share (#202).
    pub fn size_pct_skip_count(&self) -> u64 {
        self.size_pct_skipped
    }

    /// The per-entry budget in USD that is actually in force for one strategy:
    /// `equity × pct%` scaled by the leg's weight when the relative mode is
    /// armed, else the absolute `size_usd`. Reported (panel + the sizing gate)
    /// so "what will this account commit per entry?" is a number, not a
    /// reading of the source.
    ///
    /// Scope note: this reads the `set_equity_usd` scalar, which the host
    /// keeps pinned to the ACTIVE account's ledger — this is the panel's
    /// active-account budget view. Actual ticket sizing is scope-correct
    /// regardless: `equity_shares` sizes each candidate against that
    /// candidate's own account equity, never this scalar.
    pub fn entry_budget_usd(&self, strategy: &str) -> Decimal {
        let sizing = self.effective_sizing(strategy);
        if sizing.size_pct <= Decimal::ZERO {
            return sizing.size_usd;
        }
        let raw = self.equity_usd.max(Decimal::ZERO) * sizing.size_pct / Decimal::ONE_HUNDRED;
        match sizing.size_weight {
            Some(w) => (raw * w).max(Decimal::ZERO).min(raw),
            None => raw,
        }
    }

    pub fn scanner(&self) -> &Scanner {
        &self.scanner
    }

    /// 镜像盘显示簿的只读视图（行情装配层专用）：该 (venue, duration) 族
    /// 的批次被 token 唯一键遮蔽时，这里能取到该所自己申报的整批行。交易
    /// 面没有任何路径读它 —— 镜像盘与被镜像所共享 token，一份表行就是全
    /// 部交易事实；镜像归属裁决读 `mirror_of` 自声明（#643）。
    pub fn shadow_markets_for(&self, venue: &str, duration_sec: i64) -> Option<&[CryptoMarket]> {
        self.shadow_markets
            .get(&(venue.to_string(), duration_sec))
            .map(Vec::as_slice)
    }

    /// The scanner market a token belongs to (either side), if any. Multi-plugin
    /// callers (the rollover taker-kill) need the VENUE of a leg's token.
    pub fn market_for_token(&self, token: &str) -> Option<&crate::model::CryptoMarket> {
        self.scanner.market_for_token(token)
    }

    pub fn set_config(&mut self, cfg: EngineConfig) {
        self.scanner.set_config(cfg.scanner.clone());
        for s in &mut self.strategies {
            s.strategy.on_config(&cfg.trend, &cfg.spread_arb);
        }
        self.cfg = cfg;
    }

    /// Move the per-entry share band, in memory, without a restart (#191).
    ///
    /// Deliberately NOT `set_config`: that one re-notifies every hosted strategy
    /// through `on_config`, a side effect no sizing change asked for. The band is
    /// read live out of `cfg` on every ticket (`compute_shares` →
    /// `effective_sizing` → `global_sizing`), so writing these two fields is the
    /// whole change — and it remains the global ceiling that per-strategy
    /// overrides are clamped by, which is what makes it safe to move at runtime.
    pub fn set_share_band(&mut self, min_shares: Decimal, max_shares: Decimal) {
        self.cfg.min_shares = min_shares;
        self.cfg.max_shares = max_shares;
    }

    /// Tokens whose trend is currently confirmed (diagnostics/tests). The union
    /// over registered strategies.
    pub fn confirmed_tokens(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for s in &self.strategies {
            out.extend(s.strategy.confirmed_tokens());
        }
        out
    }

    /// Diagnostics: for each confirmed token, its current mid and whether it
    /// currently satisfies the entry band (per strategy).
    pub fn confirmed_diagnostics(&self, now_ms: i64) -> Vec<serde_json::Value> {
        // E3: the diagnostic ctx's round clock resolves on the TABLE's own
        // family — every row through `row_family_duration`, read on the legacy
        // venue bucket exactly like the global clock. No candidate exists at
        // this call (diagnostics walks strategies, not signals), so the table
        // shape is the best family fact here; a single-family table — every
        // corpus, and the per-family boot instances — resolves to ONE
        // duration, byte-identical with the old global read.
        let round = self.table_round_timing(now_ms);
        let fresh =
            |token: &str| fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms);
        let ctx = StrategyCtx::new(
            self.scanner.markets(),
            round.slot,
            round.time_left_sec,
            now_ms,
            &fresh,
        );
        self.strategies
            .iter()
            .flat_map(|s| s.strategy.diagnostics(&ctx))
            .collect()
    }

    pub fn book_snapshot(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        self.books
            .get(token_id)
            .filter(|b| !b.is_empty())
            .map(|b| b.snapshot(token_id))
    }

    /// Feed one market-data event. Returns tokens whose confirmed trend broke so
    /// the caller can cancel their resting bids.
    pub fn on_data(&mut self, ev: DataEvent) -> Vec<(String, Decimal)> {
        match ev {
            DataEvent::Book {
                token_id,
                bids,
                asks,
                now_ms,
            } => {
                // The book for an in-flight token almost always exists already
                // (markets arrive before their books); a get_mut hit avoids the
                // owned-key String that `entry` would allocate per event.
                let b = match self.books.get_mut(&token_id) {
                    Some(b) => b,
                    None => self.books.entry(token_id.clone()).or_default(),
                };
                b.apply_snapshot(&bids, &asks, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
                self.feed_klines(&token_id, &snap, now_ms);
            }
            DataEvent::Books { .. } => {
                // 盘口批次原子化：批事件不在本 match 里展开 —— 撕裂的根就是
                // 逐 token 的独立 apply。归档回放（`k:"books"` 批行 →
                // `event_from_json` → 本函数）与任何直接喂入都拿到与
                // [`Engine::on_data_books`] 同一条整批路径：一次调用顺序应用，
                // 策略永远看到同一批快照。静默吞掉（旧实现）会让回放里整批
                // tick 消失——比撕裂更坏的假象。
                let books = match ev {
                    DataEvent::Books { books, now_ms } => (books, now_ms),
                    _ => unreachable!("matched Books arm"),
                };
                let (books, now_ms) = books;
                return self.on_data_books(&books, now_ms);
            }
            DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                now_ms,
            } => {
                let b = self.books.entry(token_id.clone()).or_default();
                b.update_top(best_bid, best_ask, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
                self.feed_klines(&token_id, &snap, now_ms);
            }
            DataEvent::Spot {
                asset,
                price,
                now_ms,
            } => {
                self.spot
                    .entry(asset.clone())
                    .or_insert_with(|| PriceBuffer::new(180))
                    .push(price, now_ms);
                self.feed_kline_price(&asset, price, now_ms);
                return Vec::new();
            }
            DataEvent::RoundMarkets { markets, now_ms } => {
                // #498: a venue can list the round's markets STAGGERED — Kalshi's
                // four crypto assets open minutes apart, and a discovery that
                // locks the slot on its first non-empty tick then feeds 2 of 4.
                // The late listings arrive as a SECOND RoundMarkets for the SAME
                // round_slot: a full rollover replay here would wipe
                // `pending_tokens` (re-arming duplicate entries mid-round) and
                // restart the candidates-stable window for nothing. So a
                // same-slot event MERGES: append the markets the scanner does
                // not carry yet and hand the strategy layer only the genuinely
                // new books — no `on_round` re-fire, no pending/candidate
                // reset, no clock change (the first feed already observed the
                // round's one expiry). A different slot is a real rollover and
                // keeps the full replay below.
                //
                // Multi-plugin data plane: the check is VENUE-BLIND slot
                // membership (`.first()` generalized to `.any()`) — in a
                // mixed-venue table the scanner's first market may be any
                // venue's, and a same-slot feed from a SECOND venue is exactly
                // the same staggered-listing top-up (#498's "迟上架轮内补查",
                // now across venues). Single-venue tables hold one slot, so
                // `any` == the old `first` check — #498's corpus and the
                // pre-venue baselines stay byte-identical.
                //
                // B1c: slot numbers are only comparable WITHIN one duration
                // family — the 900s grid's slot 5 and the 3600s grid's slot 5
                // are two different wall-clock rounds (9000s apart). The
                // membership test is therefore (slot, duration) equality: the
                // feeding batch's own declared `round_duration_sec` (0 =
                // legacy archive → the configured global grid) must match the
                // resident row's. Single-family deployments resolve every row
                // and every batch to the SAME duration, so the added
                // condition is constant-true there and the verdict is
                // bit-identical with the pre-B1c slot-only check.
                let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
                let feeding_venue = markets.first().map(|m| m.venue.clone()).unwrap_or_default();
                let global_duration_sec = self.cfg.scanner.round_duration_sec;
                let feed_duration = row_family_duration(
                    markets.first().map(|m| m.round_duration_sec).unwrap_or(0),
                    global_duration_sec,
                );
                // 多场馆同格网（2026-10-25 poly 轮钟冻结定谳）：每个插件的
                // discovery 按自己的节奏喂自己的轮。一类批次必须丢：**slot
                // 落后于同族驻留 slot、且自身窗口已经过期**——那是「迟到喂的
                // 已过期轮」，收下它会把整张表拖回旧轮，抹掉另一个 venue 的
                // discovery 已经落好的新轮行（A 场馆迟喂过期轮 ↔ B 场馆喂
                // 新轮，ping-pong 互相覆写，轮钟冻在 00:00）。只落后但窗口
                // 还活着的批次要收：未来档先落、当前交易轮后到是生产 discovery
                // 的正常形状（对抗性喂序测试钉的就是它）。同 slot 走下面的
                // 合流，更新 slot 走下面的 rollover。单族语料 slot 单调且
                // 批次窗口未过期，此守卫恒不触发——回放逐字节不变。
                let resident_family_slot = self
                    .scanner
                    .markets()
                    .iter()
                    .filter(|m| {
                        row_family_duration(m.round_duration_sec, global_duration_sec)
                            == feed_duration
                    })
                    .map(|m| m.round_slot)
                    .max()
                    .unwrap_or(0);
                let batch_expired = markets
                    .iter()
                    .map(|m| m.expires_at_ms)
                    .min()
                    .map(|end| end <= now_ms)
                    .unwrap_or(false);
                if resident_family_slot > slot && batch_expired {
                    tracing::warn!(
                        venue = %feeding_venue,
                        batch_slot = slot,
                        resident_slot = resident_family_slot,
                        "stale round batch dropped — an expired slot behind the resident family"
                    );
                    return Vec::new();
                }
                let scanner_holds_slot = slot != 0
                    && self.scanner.markets().iter().any(|m| {
                        m.round_slot == slot
                            && row_family_duration(m.round_duration_sec, global_duration_sec)
                                == feed_duration
                    });
                if scanner_holds_slot {
                    // The batch's own venue tag + its expiry, read BEFORE the
                    // fresh filter consumes the list — the fresh-empty path
                    // below still needs both (a mirror venue's batch carries
                    // its identity and declaration even when every token is
                    // already known).
                    let batch_venue_tag =
                        markets.first().map(|m| m.venue.clone()).unwrap_or_default();
                    let batch_first_expiry =
                        markets.first().map(|m| m.expires_at_ms).unwrap_or_default();
                    let known: std::collections::HashSet<&str> = self
                        .scanner
                        .markets()
                        .iter()
                        .flat_map(|m| [m.up_token_id.as_str(), m.down_token_id.as_str()])
                        .collect();
                    let fresh: Vec<CryptoMarket> = markets
                        .iter()
                        .filter(|m| {
                            !known.contains(m.up_token_id.as_str())
                                && !known.contains(m.down_token_id.as_str())
                        })
                        .cloned()
                        .collect();
                    if fresh.is_empty() {
                        // Pure duplicate: nothing to add, nothing to restart.
                        //
                        // 镜像盘（2026-10-25 poly 轮钟冻结定谳）：「全已知」
                        // 不等于「同一家的重复申报」——PolySimulator 镜像
                        // Polymarket，同窗口 token 逐字节相同；PM 批先落，
                        // 这批就被判成纯重复。两条不可再吞的伴生事实：
                        // ① 轮钟——该 (venue, duration) 若还没开口，这次
                        //   申报就是它唯一的发言机会（它的 next feed 在
                        //   下个窗口，仍是 merge 路径），静默 return 会让
                        //   venueRounds 永远缺行、面板倒计时绑不上；observed
                        //   幂等检查挡掉真正的同 venue 重复申报。
                        // ② 显示簿——表按 token 唯一，它的行进不了表，把
                        //   整批存进 shadow_markets：行情装配层据此给该所
                        //   合成带自己标签的行情行。交易面不读这张簿。
                        //
                        // observe 无条件照写（同换轮臂的信任序：批次自带的
                        // 申报就是该所自己的权威，越界值由 observe 内部的
                        // 2 轮 guard 拒收）——不按「已开口」跳过：上窗口的
                        // 开口挡不住本窗口的新申报，跳过会把钟永远钉在旧
                        // 轮。同所真重复申报重写同一个值，状态逐字节不变。
                        self.scanner.observe_end_time_for(
                            &batch_venue_tag,
                            feed_duration,
                            batch_first_expiry,
                            now_ms,
                        );
                        let has_own_rows = self.scanner.markets().iter().any(|m| {
                            m.venue == batch_venue_tag
                                && row_family_duration(m.round_duration_sec, global_duration_sec)
                                    == feed_duration
                        });
                        if !has_own_rows {
                            self.shadow_markets
                                .insert((batch_venue_tag, feed_duration), markets);
                        }
                        return Vec::new();
                    }
                    // Seed the genuinely new tokens from any book that has
                    // already arrived (the descriptors usually precede their
                    // first book event) BEFORE the scanner grows: the borrow
                    // of the merged list ends with `set_markets`, and the
                    // seeds only need `fresh`.
                    for t in fresh
                        .iter()
                        .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    {
                        if let Some(snap) = self.book_snapshot(&t) {
                            for s in &mut self.strategies {
                                s.strategy.on_book(&t, &snap, now_ms);
                            }
                        }
                    }
                    // Observe the feeding batch's own expiry on the merge path
                    // too. The comment above ("the first feed already observed
                    // the round's one expiry") is a single-venue assumption:
                    // in a mixed-venue table the staggered top-up is usually
                    // the SECOND venue's ONLY feed of the round — its first
                    // `RoundMarkets` lands here, never at the rollover — so
                    // skipping the observation would leave it out of
                    // `observed_venue_keys()` for the whole round: no panel
                    // `venueRounds` row (the countdown rebind waits for it and
                    // shows 00:00 on an account switch to that venue) and its
                    // timing gate reading the sibling's clock. Same discipline
                    // as the rollover arm: the declaration lands in ITS OWN
                    // venue slot and out-of-band values are refused inside
                    // `observe_end_time_for_venue`.
                    let merge_venue = fresh.first().map(|m| (m.venue.clone(), m.expires_at_ms));
                    let mut merged = self.scanner.markets().to_vec();
                    merged.extend(fresh);
                    self.scanner.set_markets(merged);
                    if let Some((venue, end_ms)) = merge_venue {
                        // B1c: the declaration lands in the FEEDING FAMILY's
                        // (venue, duration) clock slot, not the global-grid
                        // slot — a 1h top-up must not speak into the 15m
                        // family's clock. Single-family feeds resolve
                        // `feed_duration` to the configured grid, the same
                        // key the legacy entry wrote.
                        self.scanner
                            .observe_end_time_for(&venue, feed_duration, end_ms, now_ms);
                    }
                    return Vec::new();
                }
                // ── #550 复现缺陷（症状①）的根因之一：无界 sibling 存活 ──
                // 换轮臂（下方）只替换 FEEDING venue 的行，兄弟行「存活到
                // 自己的 feed 来换」是 staggered 上架的桥接语义（兄弟在轮
                // 界刚过、还没重报的几秒里不能丢行），但没有边界：兄弟停喂
                // 后它的行带着已过期的 expiry 永久留在表里 —— 行情页渲染
                // 已结束轮次的行，它的 observed expiry 还通过全局轮钟的
                // `earliest_observed` 把 time_left 钉死 0（全体 Timing 门
                // 拦死新开仓）。
                //
                // 处置：错满一整轮（round_slot 距本批 slot ≥ 2）的兄弟行
                // 清出表；只差一格的照旧桥接（下方 per-venue 替换照常处置
                // feeding venue 自己的行，这里不重复动它）。venue-blind 的
                // merge 判定按 (slot, duration) 相等匹配（B1c），落后行永不
                // 命中 merge 路径，所以这不改变 merge/rollover 的判定本身
                // —— 只给「存活」加一条可退出的边界。
                //
                // B1c（#550 按族网格）：slot 数字跨 duration 族不可比 ——
                // 900s 网格的 slot N 与 3600s 网格的 slot N 相差数小时。
                // 「落后几格」的判定因此按每行自己的 duration 网格逐行算
                // （行带 round_duration_sec 用它，否则全局轮长），而不是
                // 一根全局尺。单族表里每行解析出同一个 duration，逐行判定
                // 与旧的全局判定逐字节等价。
                if slot != 0 {
                    let mut kept: Vec<CryptoMarket> = self
                        .scanner
                        .markets()
                        .iter()
                        .filter(|m| {
                            // B1c: the lag is measured on the ROW's OWN grid —
                            // the feeding round's start (slot × feed duration,
                            // seconds) converted onto that row's duration
                            // before the distance is taken. Slot numbers are
                            // not comparable across duration families (the
                            // 900s grid's slot 100 and the 3600s grid's slot
                            // 100 are hours apart); a 0-duration row (legacy
                            // archive) rides the global grid — the same
                            // resolution the merge test uses. Same-family rows
                            // (the only rows a single-family table holds)
                            // resolve to the historic `slot - round_slot`
                            // verbatim.
                            let row_dur =
                                row_family_duration(m.round_duration_sec, global_duration_sec)
                                    .max(1);
                            let lag = if row_dur == feed_duration {
                                slot - m.round_slot
                            } else {
                                slot * feed_duration / row_dur - m.round_slot
                            };
                            lag < 2 || m.venue == feeding_venue
                        })
                        .cloned()
                        .collect();
                    if kept.len() != self.scanner.markets().len() {
                        let dropped = self.scanner.markets().len() - kept.len();
                        tracing::info!(
                            target: "engine",
                            "round rollover: dropping {dropped} stale market row(s) from \
                             venue(s) that missed a full round (slot < {}), feeding venue \
                             {feeding_venue} excluded",
                            slot - 1
                        );
                        self.scanner.set_markets(std::mem::take(&mut kept));
                    }
                }
                // The venue's own round declaration beats the local slot grid:
                // `expires_at_ms` is the exchange's actual round end, and
                // `time_left_sec` (the exit policy's and the D-31 floor's
                // clock) must track it, not the host's wall clock. Without
                // this, a misaligned or shifted venue schedule silently
                // drifts every timing decision. `observe_end_time` ignores
                // non-positive declarations and derives the clock offset —
                // into the feeding market's OWN venue slot (multi-plugin:
                // Kalshi's declaration must not move Polymarket's clock).
                if let Some(m) = markets.first() {
                    // B1c: the feeding batch's declaration speaks for ITS OWN
                    // (venue, duration) family — a 1h rollover must not move
                    // the 15m family's clock.
                    self.scanner.observe_end_time_for(
                        &m.venue,
                        feed_duration,
                        m.expires_at_ms,
                        now_ms,
                    );
                }
                // Real round timing for the transition: how much of the round
                // was left at this instant, straight from the scanner. The
                // same-batch venue's own clock (a rollover REPLACES that
                // venue's markets — its old expiry is gone with them; the
                // conservative global window still covers the siblings).
                // B1c: the family's own clock — the feeding family's duration,
                // not the global grid, decides the slot boundary. In a
                // single-family deployment this is the legacy entry verbatim.
                let venue = feeding_venue.clone();
                let time_left_sec = self
                    .scanner
                    .round_timing_for(&venue, feed_duration, now_ms)
                    .time_left_sec;
                for s in &mut self.strategies {
                    s.strategy.on_round(slot, time_left_sec, now_ms);
                }
                // ── B1c：rollover 只重置 feeding (venue, duration) 族 ──
                // 这三条状态（pending 去重、staged 候选、稳定相位）是
                // 「这一轮的入场」的事实。升级前全局轮长一根尺：任何 venue
                // 换轮都全体清空 —— 单族正确；多族下 1h 轮的翻转会把 15m
                // 族轮内刚挂上的 pending 清掉（重新武装重复入场）并作废
                // 别族 staged 候选。scanner 侧喂钟（B1a）已按 (venue,
                // duration) 键控，engine 侧 reset 对齐同族：只有本族 token
                // /本族候选出局，别族不动。
                //
                // 本族 token 集：本批换轮后的新行 token ∪ 表里同族旧行
                // token —— 同 (venue,duration) 的全部行（不分 slot 数字：
                // 落后一兩格的同族兄弟行是 staggered 桥接里的旧轮行，其轮
                // 已换、其 pending 同样必须出局）就是「这个族的入场事实」
                // 的全部对象；别族（不同 duration）的行不在集合里，它们的
                // pending/staged 候选原样不动。喂入行无 venue 标签（legacy
                // 单插件）时整表就是本族，集合即全表 —— 与旧的全清逐字节
                // 等价。
                let family_tokens: HashSet<String> = {
                    let mut set: HashSet<String> = HashSet::new();
                    for m in &markets {
                        set.insert(m.up_token_id.clone());
                        set.insert(m.down_token_id.clone());
                    }
                    for m in self.scanner.markets() {
                        if row_family_duration(m.round_duration_sec, global_duration_sec)
                            == feed_duration
                        {
                            set.insert(m.up_token_id.clone());
                            set.insert(m.down_token_id.clone());
                        }
                    }
                    set
                };
                self.pending_tokens
                    .retain(|(_, tok)| !family_tokens.contains(tok));
                // Issue #390 (replay nofast): the new round retires every
                // staged candidate — the base's next collection ran against
                // the new market list and round clock, so a replayed set from
                // the old round must not leak into it. B1c: only candidates
                // of the FEEDING FAMILY retire — a sibling family's staged
                // set (its tokens still live, its clock still running) stays
                // staged; single-family tables lose everything, byte-identical
                // with the unscoped clear.
                self.last_candidates
                    .retain(|sig| !family_tokens.contains(&sig.token_id));
                // A set that survived the family cut is a candidate set the
                // base's next collection can still reproduce (its inputs —
                // the sibling family's books, clock and round list — did not
                // change with THIS family's rollover), so the stability flag
                // keeps describing it. The historic whole-clear emptied the
                // set, and an empty staged set was exactly "not stable"; the
                // empty-set verdict falls out naturally here too.
                self.candidates_stable = self.candidates_stable && !self.last_candidates.is_empty();
                // The rollover replaces the FEEDING VENUE's markets only: a
                // batch always lists the complete round of its own venue, so
                // everything of `venue` in the table is stale. Sibling venues'
                // markets (multi-plugin data plane) stay live — they roll over
                // through their own feeds. An empty venue tag keeps the whole
                // table replace (the historical single-plugin semantics).
                // B1c: "everything of the venue" means the venue's rows of the
                // FEEDING FAMILY — a same-venue sibling family (PM 15m rows
                // beside PM 1h rows) is live in its own round and is replaced
                // by its OWN feed, never collateral of this one. Single-family
                // tables resolve every row to the feed's duration, so the
                // predicate degenerates to the historic `m.venue != venue`.
                if venue.is_empty() {
                    self.scanner.set_markets(markets);
                } else {
                    let mut kept: Vec<CryptoMarket> = self
                        .scanner
                        .markets()
                        .iter()
                        .filter(|m| {
                            m.venue != venue
                                || row_family_duration(m.round_duration_sec, global_duration_sec)
                                    != feed_duration
                        })
                        .cloned()
                        .collect();
                    kept.extend(markets);
                    self.scanner.set_markets(kept);
                }
                // Seed trend with current mids so confirmation starts immediately.
                let now = now_ms;
                let tokens: Vec<String> = self
                    .scanner
                    .markets()
                    .iter()
                    .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    .collect();
                for t in tokens {
                    if let Some(snap) = self.book_snapshot(&t) {
                        for s in &mut self.strategies {
                            s.strategy.on_book(&t, &snap, now);
                        }
                    }
                }
            }
            DataEvent::RoundEnd { now_ms: _ } => {
                // #352: boundary marker only — the rollover that actually
                // replaces the markets is `RoundMarkets`; a replay consumer
                // uses RoundEnd to close the round's book. Nothing to trade on.
                return Vec::new();
            }
            DataEvent::Trade { .. } => {
                // #352: an on-chain print is information, not a book update —
                // strategies decide on books and spot only.
                return Vec::new();
            }
            DataEvent::Resolution { .. } => {
                // #377 (治 #2): a verdict is settlement input for the Core,
                // not a book update — no strategy acts on it here.
                return Vec::new();
            }
            DataEvent::Cashflow { .. } => {
                // #388: a wallet-level cash row is ledger input for the Core,
                // not a book update — no strategy acts on it here.
                return Vec::new();
            }
            DataEvent::UnifiedMapping { .. } => {
                // #427: a confirmed cross-venue mapping is strategy-layer
                // state (which tokens across venues are one bet), not a book
                // update. The core keeps the latest mapping per event id so
                // the strategy layer can read it through the view; the match
                // arm stays inert to trading.
            }
        }
        self.drain_breaks()
    }

    /// 盘口批次原子化：整批 book 在同一次调用里顺序应用，每一本走与
    /// [`DataEvent::Book`] 完全相同的 apply → 逐策略 `on_book` → near-miss →
    /// K-line 路径，策略因此永远看到同一批快照（时间戳一致、新旧一致），
    /// 不存在「批内一半新一半旧」的中间态。批次时间戳 `now_ms` 由调用方
    /// 统一给定（批式拉取一次请求一个时钟）。返回聚合的趋势断裂供调用方
    /// 撤单（与 [`Engine::on_data`] 的返回语义相同）。
    pub fn on_data_books(&mut self, books: &[BatchBook], now_ms: i64) -> Vec<(String, Decimal)> {
        for (token_id, bids, asks) in books {
            let b = match self.books.get_mut(token_id) {
                Some(b) => b,
                None => self.books.entry(token_id.clone()).or_default(),
            };
            b.apply_snapshot(bids, asks, now_ms);
            let snap = b.snapshot(token_id);
            for s in &mut self.strategies {
                s.strategy.on_book(token_id, &snap, now_ms);
            }
            self.near_miss.on_book(token_id, &snap, now_ms);
            self.feed_klines(token_id, &snap, now_ms);
        }
        self.drain_breaks()
    }

    /// E29 (§10.3): fold one book-derived price into the K-line aggregator.
    ///
    /// The venue publishes no trade tape this deployment can read, so the bar
    /// is built from the book MID at the event's own timestamp — the same
    /// price the exit policy and the trend filter already decide on, not a
    /// new feed and not a guess. One tick per `(symbol, interval)` per event;
    /// the aggregator owns bucketing, out-of-order drops and closing.
    ///
    /// `_now_ms` is deliberately the EVENT timestamp (`now_ms` carried by the
    /// DataEvent), not the host wall clock: K-line bucketing runs on venue
    /// time, the clock every other decision here already runs on.
    fn feed_klines(&mut self, symbol: &str, snap: &OrderbookSnapshot, ts_ms: i64) {
        self.feed_kline_price(symbol, snap.mid_price, ts_ms);
    }

    /// E29: fold one price tick (book mid or spot print) into the aggregator
    /// and dispatch the bars it closed. Non-positive prices are skipped: an
    /// empty or one-sided book has no honest price to fold.
    fn feed_kline_price(&mut self, symbol: &str, price: Decimal, ts_ms: i64) {
        if price <= Decimal::ZERO {
            return;
        }
        let closed = self.klines.on_trade(symbol, price, Decimal::ONE, ts_ms);
        if closed.is_empty() {
            return;
        }
        // §10.4: strategies see CLOSED bars only, in closure order.
        for s in &mut self.strategies {
            for bar in &closed {
                s.strategy.on_kline(bar);
            }
        }
    }

    /// E29: bars closed since the last drain — the host's `KLINE_UPDATE`
    /// source. Drained exactly once (§12.2: a closed bar MUST be pushed).
    pub fn take_closed_klines(&mut self) -> Vec<crate::kline::Kline> {
        self.klines.take_closed()
    }

    /// E29: the still-growing bars, for the throttled "正在长" push.
    pub fn kline_current_bars(&self) -> Vec<crate::kline::Kline> {
        self.klines.current_bars()
    }

    /// E29: `kline.history` body — closed tail + the growing bar last.
    pub fn kline_history(
        &self,
        symbol: &str,
        interval: crate::kline::KlineInterval,
        limit: usize,
    ) -> Vec<crate::kline::Kline> {
        self.klines.history(symbol, interval, limit)
    }

    /// E29: aggregation counters (drops must be visible, never silent).
    pub fn kline_stats(&self) -> crate::kline::AggregatorStats {
        self.klines.stats()
    }

    /// Collect trend breaks from every strategy (each break is cancelled once).
    ///
    /// Every hosted strategy is DRAINED, but only the enabled ones contribute. The
    /// hook still runs on a disabled strategy because its library may be queueing
    /// breaks internally and the call is what empties that queue — skipping it
    /// would let the queue grow for as long as the strategy stays off. The breaks
    /// are then dropped rather than returned: a break's only effect is cancelling
    /// this token's resting entry bids, and a disabled strategy must not change
    /// another strategy's fills (#302 — `trend_follow` registered-but-off still
    /// pulled `spread_arb`'s bids, so the fills depended on which libraries
    /// happened to sit in the strategy directory).
    fn drain_breaks(&mut self) -> Vec<(String, Decimal)> {
        let mut out = Vec::new();
        for s in &mut self.strategies {
            let breaks = s.strategy.take_breaks();
            if s.enabled {
                out.extend(breaks);
            }
        }
        out
    }

    /// Spot momentum alignment filter: reject when spot is moving against the bet.
    /// Returns the reason it rejected, so an exempting strategy's audit record can
    /// state exactly what was waived.
    fn momentum_ok(&self, asset: &str, dir: SignalDirection, now_ms: i64) -> Result<(), String> {
        let Some(buf) = self.spot.get(asset) else {
            return Ok(());
        };
        let move_pct = buf.move_pct(self.cfg.momentum_window_sec, now_ms);
        let against = match dir {
            SignalDirection::Up => move_pct < -self.cfg.momentum_tol_pct,
            SignalDirection::Down => move_pct > self.cfg.momentum_tol_pct,
        };
        if against {
            return Err(format!(
                "spot {asset} {move_pct:+.2}% in {}s vs {} (tol {}%)",
                self.cfg.momentum_window_sec,
                dir.as_str(),
                self.cfg.momentum_tol_pct
            ));
        }
        Ok(())
    }

    /// Evaluate all enabled strategies against the current round and return the
    /// entry orders that should be placed. Pure w.r.t. the core: the caller
    /// places them and reports back tokens that got an order so repeats are
    /// suppressed this round.
    pub fn evaluate(&mut self, now_ms: i64) -> Vec<crate::model::OrderRequest> {
        self.last_blocked.clear();
        self.last_exemptions.clear();
        // RFC 568 阶段一：配对声明与拒绝回执都是「本周期」的伴生事实 ——
        // 每次真实评估重开（Core 在 precheck 过闸后消费 take_pair_declarations，
        // 拒绝回执经 take_pair_refusals 转 APPLICATION 事件）。
        self.pair_declarations.clear();
        self.pair_refusals.clear();

        // E3: the cycle's round clock reads the TABLE's own family (the
        // legacy venue fold, duration refined) instead of the configured
        // grid — single-family tables resolve to the same numbers, a mixed
        // table stops reporting 15m-grid slot/time_left to strategies
        // pricing other durations. The per-candidate refinements below use
        // each candidate's OWN market family.
        let round = self.table_round_timing(now_ms);
        // E25: the arbitration pipeline's physics projection reads the SAME
        // round timing this cycle was judged against.
        self.last_time_left_sec = round.time_left_sec;

        // Candidates are computed regardless of the timing gate so we can record
        // near-misses (a valid dip that the timing gate blocked). This is the data
        // the shadow log lacks and the --replay entry analysis needs.
        let mut candidates: Vec<TradeSignal> = Vec::new();
        let mut exit_intents: Vec<crate::strategies::StrategyExitIntent> = Vec::new();
        // #427: the mapping snapshot for this cycle — every strategy sees the
        // SAME confirmed events (state of the world, not per-strategy data).
        let unified_snapshot: Vec<blitzkrieg_market_api::unified::UnifiedEvent> =
            self.unified_mappings.values().cloned().collect();
        {
            let fresh = |token: &str| {
                fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms)
            };
            // The host-pushed position snapshot, sliced per scope inside the
            // loop: each scope's ctx carries ONLY its own positions — same
            // strategy on two accounts is two independent books, and neither
            // package may read the other's holdings through `bk.holdings()`.
            // 跨所套利弹窗 v2 exception: a strategy that trades under the arb
            // leg configuration STAMPS each leg to ITS OWN venue's account
            // (entries and exits alike), so one instance's book legitimately
            // spans the configured leg accounts — the release decision prices
            // the WHOLE pair and must see both legs' held rows, or a B-leg
            // exit would be suggested against a holding the instance cannot
            // see (the leg would be trapped until settlement). The widening
            // is bounded by the configuration itself: only rows whose account
            // is one of the two leg accounts join the view, so unrelated
            // scopes' books stay sealed.
            for s in self.strategies.iter_mut() {
                let leg_accounts = s.strategy.arb_leg_accounts();
                if s.enabled {
                    let held: Vec<HeldPosition> = self
                        .positions_view
                        .iter()
                        .filter(|p| {
                            p.strategy == s.strategy.name()
                                && (p.account_id == s.account_id
                                    || leg_accounts
                                        .as_ref()
                                        .is_some_and(|ls| ls.contains(&p.account_id)))
                        })
                        .cloned()
                        .collect();
                    // RFC 568 阶段一：同样的 per-scope 切片，配对簿视图 ——
                    // 只按 owner 策略切（账户白名单过滤 Core 侧已做完）。
                    let pair_views: Vec<crate::pair::PairView> = self
                        .pair_views_snapshot
                        .iter()
                        .filter(|p| p.owner_strategy == s.strategy.name())
                        .cloned()
                        .collect();
                    let ctx = StrategyCtx::new(
                        self.scanner.markets(),
                        round.slot,
                        round.time_left_sec,
                        now_ms,
                        &fresh,
                    )
                    .with_positions(&held)
                    .with_unified_events(&unified_snapshot)
                    .with_pair_views(&pair_views);
                    candidates.extend(s.strategy.find_candidates(&ctx));
                    // Close intents accumulate regardless of the entry timing
                    // gate: an open position's exit is never round-timing-gated.
                    exit_intents.extend(s.strategy.take_exit_intents());
                }
            }
        }
        // Issue #390 (replay nofast): stage what this real evaluation
        // produced, and decide the replay licence. The inputs behind the set
        // are event-driven, so between events it cannot change — a stable
        // set (identical to the previous real evaluation's) is the state a
        // quiet replay cycle reproduces; a changed set is a phase transition
        // (a latch fired, an emission was consumed) whose NEXT evaluation
        // must be real, because the base's next collection would collect the
        // post-transition output this cycle already produced.
        self.candidates_stable = self.last_candidates == candidates;
        self.last_candidates = candidates.clone();
        self.strategy_exits.extend(exit_intents);
        self.submit_candidate_orders(candidates, now_ms)
    }

    /// Issue #390 (replay nofast): a quiet-tick evaluation cycle. Runs the
    /// STANDING candidates of the most recent real evaluation through the
    /// SAME kernel-side pipeline the real cycle used — the pending filter,
    /// the live round-timing and momentum gates (with their near-miss and
    /// exemption trails), sizing, and order construction — mirroring the
    /// base walk's every-tick behaviour: the base re-collected these
    /// suggestions from unchanged inputs every tick and re-ran them into the
    /// same gates, so the replay reproduces its counters, logs and placement
    /// instants cycle for cycle without a Lua call. Deliberately NOT staged
    /// back into `last_candidates`: the staged set is a real evaluation's
    /// output, and every real evaluation (event wake, timing boundary, 2 s
    /// fallback) refreshes it. Returns None when the staged set is not in a
    /// stable phase (`candidates_stable == false`) — the caller must run a
    /// real evaluation instead, because the strategy's next collection would
    /// produce a post-transition output this cache cannot know.
    pub fn replay_candidates(&mut self, now_ms: i64) -> Option<Vec<crate::model::OrderRequest>> {
        if !self.candidates_stable {
            return None;
        }
        self.last_blocked.clear();
        self.last_exemptions.clear();
        // E3: same family resolution as the real evaluation — the table's
        // own duration on the legacy venue fold (single-family corpora read
        // the same numbers; the per-candidate refinements inside
        // `submit_candidate_orders` are each candidate's own family).
        let round = self.table_round_timing(now_ms);
        self.last_time_left_sec = round.time_left_sec;
        // The freshness rule is the one collection input that moves with the
        // clock alone (`now_ms - book.timestamp`): a book that has gone stale
        // since the staging evaluation drops its suggestion, exactly as the
        // base's re-collection at this tick would not have produced it.
        let round_tokens: HashSet<String> = self.round_token_ids().into_iter().collect();
        let candidates: Vec<TradeSignal> = self
            .last_candidates
            .iter()
            .filter(|sig| {
                round_tokens.contains(&sig.token_id)
                    && fresh_book(
                        &self.books,
                        &sig.token_id,
                        now_ms,
                        self.cfg.max_orderbook_stale_ms,
                    )
                    .is_some()
            })
            .cloned()
            .collect();
        Some(self.submit_candidate_orders(candidates, now_ms))
    }

    /// Issue #390 (replay driver wake): true when at least one staged
    /// candidate would survive the pending filter and therefore be re-run
    /// into the kernel-side gates on the next cycle — the state a skipped
    /// grid tick would NOT reproduce. Stability is deliberately NOT part of
    /// this verdict: an unstable set makes the next cycle a REAL evaluation
    /// (which re-collects and re-submits the same retry stream), a stable one
    /// makes it a replay — either way the base walk ran it on every tick, so
    /// the driver must not skip. Pending-covered candidates are excluded: the
    /// submission path drops them before any gate, counter or log line, so
    /// their quiet-tick re-emission is unobservable (the base proved this by
    /// running it every tick) and they must not pin the driver to the
    /// every-tick walk.
    pub fn has_standing_candidates(&self) -> bool {
        self.last_candidates.iter().any(|sig| {
            !self
                .pending_tokens
                .contains(&(sig.account_id.clone(), sig.token_id.clone()))
        })
    }

    /// The kernel-side half of one evaluation cycle: pending/dedup filters,
    /// the LIVE round-timing and momentum gates (near-miss and exemption
    /// trails included), sizing, and order construction. Shared verbatim by
    /// the real evaluation and the replay's quiet-tick cycles — one code
    /// path, so a replayed suggestion is judged exactly as a fresh one.
    fn submit_candidate_orders(
        &mut self,
        candidates: Vec<TradeSignal>,
        now_ms: i64,
    ) -> Vec<crate::model::OrderRequest> {
        // E3: the table-level clock the per-candidate refinement falls back
        // to (a candidate whose market the scanner already dropped) — the
        // table's own family resolution, same shape as the callers read.
        let round = self.table_round_timing(now_ms);
        let mut orders = Vec::new();
        // At most one entry per token per cycle: the first candidate (in
        // registration order) wins, matching the one-live-entry-per-token rule
        // the caller enforces across cycles via `pending_tokens`.
        let mut emitted: HashSet<(String, String)> = HashSet::new();
        // ── RFC #541 方案 2a-i 挂钩 A（原型）：declared_atomic 策略的 scope
        // 判定先整组化。逐条路径里四个 scope 门（trading/frozen/exits_only/
        // admits_token）任一拒绝是「静默丢腿」——pair 的两条腿会在这里断开，
        // 第一腿照常进 OME（失败模式②）。对声明了 atomic_batch 的策略，先对
        // 本周期该策略的全部 candidates 做整组判定：任一条被 scope 门拒 ⇒
        // 该策略本周期全部 candidates 丢弃（0 腿到 OME），拒绝计数/审计保持
        // 逐条形状（见 atomic_scope_refusals 的用途），只加一条汇总 INFO。
        // 未声明 atomic_batch 的策略（spread_arb 等单所策略）不进这个分支，
        // 走下面的历史逐条路径，行为逐字节不变。pending_tokens 去重与
        // emitted 去重不是「拒绝」而是「已处理」，不参与整组判定。
        let mut atomic_scope_refusals: std::collections::BTreeMap<
            String,
            Vec<(&'static str, String)>,
        > = std::collections::BTreeMap::new();
        for sig in &candidates {
            if !self.strategy_atomic_batch(&sig.strategy) {
                continue;
            }
            // A candidate the scope gates refuse: record (gate, leg) — the
            // strategy's WHOLE cycle is then dropped below.
            let gate = if !self.scope_trading(&sig.account_id) {
                "scope_trading"
            } else if self.scope_frozen(&sig.account_id) {
                "scope_frozen"
            } else if self.scope_exits_only(&sig.strategy, &sig.account_id) {
                "scope_exits_only"
            } else if !self.scope_admits_token(&sig.account_id, &sig.token_id) {
                "scope_admits_token"
            } else {
                continue;
            };
            atomic_scope_refusals
                .entry(sig.strategy.clone())
                .or_default()
                .push((
                    gate,
                    format!(
                        "{}@{} (token {})",
                        sig.strategy, sig.account_id, sig.token_id
                    ),
                ));
        }
        // Any refused candidate ⇒ the strategy's WHOLE cycle is dropped:
        // zero legs out. Audited as one INFO line naming the gate(s) and the
        // refused legs (the per-strategy atomic batch is judged as a unit —
        // RFC #541's hook A).
        let dropped_atomic: HashSet<String> = atomic_scope_refusals.keys().cloned().collect();
        for (strategy, refusals) in &atomic_scope_refusals {
            let gates: Vec<&str> = refusals.iter().map(|(g, _)| *g).collect();
            tracing::info!(
                target: "strategy",
                "atomic batch dropped by scope filter: strategy={strategy} legs={} \
                 refused_legs={:?} gates={:?} — zero legs emitted this cycle",
                candidates.iter().filter(|s| &s.strategy == strategy).count(),
                refusals.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
                gates,
            );
        }
        // ── RFC 568 阶段一（补钉 5 的权威位）：A≠B 账户校验 —— 同一个自报
        // pair_key 下，两条腿盖出的 effective account_id 相同 ⇒ 拒绝该声明
        // （APPLICATION 错误，点名字段）。双腿落同账户的「配对」不是跨所
        // 套利（一笔单所双腿单 —— 配对簿的审批对象不存在），fail-closed 拒
        // 声明而非放行裸腿。只对带 pair 声明且未被 scope 门整组丢弃的腿判
        // 定；凭证不在这里校验（补钉 3 —— declare 在 arb_precheck 过闸之后）。
        // 校验通过 ⇒ 收进 pair_declarations，等 Core 在 precheck 过闸后 declare。
        {
            // 自报 pair_key → 该 key 下各腿的 effective account（本轮候选里）。
            let mut pair_accounts: std::collections::BTreeMap<&str, Vec<&str>> =
                std::collections::BTreeMap::new();
            for sig in &candidates {
                let Some(p) = &sig.pair else { continue };
                if dropped_atomic.contains(&sig.strategy) {
                    continue; // 整组已丢：无 effective 账户可言
                }
                pair_accounts
                    .entry(p.key.as_str())
                    .or_default()
                    .push(sig.account_id.as_str());
            }
            for sig in &candidates {
                let Some(p) = &sig.pair else { continue };
                if dropped_atomic.contains(&sig.strategy) {
                    continue;
                }
                let accounts = pair_accounts.get(p.key.as_str()).expect("seeded above");
                if accounts.len() >= 2 && accounts.iter().all(|a| *a == accounts[0]) {
                    let reason = format!(
                        "pair.declRefused: pair `{}` legs share effective account `{}` — \
                         both legs must stamp DIFFERENT accounts (a cross-venue pair, not a \
                         single-venue two-leg order); field: pair.key",
                        p.key, accounts[0]
                    );
                    tracing::info!(target: "strategy", "{reason}");
                    self.pair_refusals.push(reason);
                    continue;
                }
                self.pair_declarations.push(PairDeclaration {
                    owner_strategy: sig.strategy.clone(),
                    // 实例主账户：该策略实例自己的 scope（注册时的
                    // account_id，trait 访问器 `instance_account`），不是腿
                    // 的盖章结果 —— 实例隔离维度（跨实例同名 pair_key 不串
                    // 账）；跨同名多实例的歧义按第一实例落（与下游以
                    // (strategy, 腿账户) 寻址的判定链同口径）。
                    owner_account: self
                        .strategies
                        .iter()
                        .find(|s| s.strategy.name() == sig.strategy)
                        .map(|s| s.strategy.instance_account())
                        .unwrap_or_else(|| sig.account_id.clone()),
                    leg_account: sig.account_id.clone(),
                    decl: crate::signal::TradePairDecl {
                        key: p.key.clone(),
                        leg: p.leg.clone(),
                    },
                    token_id: sig.token_id.clone(),
                    shares: sig.shares,
                });
            }
        }
        for sig in candidates {
            if dropped_atomic.contains(&sig.strategy) {
                continue;
            }
            // Account-book facts FIRST (before any gate can record a near-miss
            // or burn a cooldown): a scope whose trading switch is OFF
            // contributes nothing, and a scope bound to venue V never prices
            // another venue's token. Exits bypass this entire path.
            if !self.scope_trading(&sig.account_id) {
                continue;
            }
            // Account-scoped kill switch (`account.freeze`): a frozen scope is
            // invisible to entries, exactly like a trading-OFF one. Exits
            // bypass this whole path, so held positions stay managed.
            if self.scope_frozen(&sig.account_id) {
                continue;
            }
            // The exits-only demotion (`account.strategies.set` safety net):
            // a removed-but-held scope keeps its exits flowing but emits no
            // NEW entries, per (strategy, account) — the same strategy on a
            // still-assigned account keeps trading normally.
            if self.scope_exits_only(&sig.strategy, &sig.account_id) {
                continue;
            }
            if !self.scope_admits_token(&sig.account_id, &sig.token_id) {
                continue;
            }
            if self
                .pending_tokens
                .contains(&(sig.account_id.clone(), sig.token_id.clone()))
            {
                continue;
            }
            // One entry per (scope, token) per cycle: account A's candidate
            // for a token never blocks account B's.
            if !emitted.insert((sig.account_id.clone(), sig.token_id.clone())) {
                continue;
            }
            // Multi-plugin data plane: the timing verdict is PER-CANDIDATE —
            // the expiry gate reads the candidate's own venue clock, so a
            // polymarket market inside its force-exit window no longer freezes
            // a kalshi candidate in the same evaluation (and vice versa).
            // Presence/age stay the global verdict; the per-market form only
            // refines which expiry applies. Without a market (stale signal for
            // a token the scanner dropped) the whole-table verdict decides.
            // E3: the same per-market refinement now carries the FAMILY — the
            // candidate's own declared duration grid (its own expiry clock
            // and its slot), so the whole gate trail below (D-31 floor,
            // near-miss telemetry, block/exemption stamps, the internal
            // key's round segment) reads numbers from the candidate's clock,
            // not the global grid. Single-family tables resolve every row to
            // the configured grid, byte-identical with the pre-E3 read.
            let cand_round = match self.scanner.market_for_token(&sig.token_id) {
                Some(m) => self.family_round_timing(m, now_ms),
                None => round,
            };
            let timing = match self.scanner.market_for_token(&sig.token_id) {
                Some(m) => self.scanner.can_trade_reason_for(m, now_ms).err(),
                None => self.scanner.can_trade_reason(now_ms).err(),
            };
            if let Some(block) = timing {
                // E2-b: a strategy may declare the timing WINDOW gates unnecessary
                // for its own candidates. The structural precondition (no market
                // this round) is not waivable, and everything downstream in
                // `Core` (risk gate, kill switch, daily loss, quotas, sizing) is
                // untouched by this path.
                //
                // D-31: the exemption is honoured only while `time_left_sec` is at
                // or above the strategy's declared floor (default: the scanner's
                // own `min_time_left_sec`). Before this, an exempt strategy could
                // enter inside the force-exit window, where the exit policy fires
                // on the very next tick — a correctly-priced but guaranteed
                // zero-hold exit that only put noise into the ledger. A "too young"
                // round has a large `time_left_sec`, so it stays waived; the block
                // this actually stops is "too close to expiry", which by definition
                // sits below the floor.
                let waived = block.exemptible()
                    && self
                        .strategy_gate_exemptions(&sig.strategy)
                        .is_some_and(|e| {
                            e.timing
                                && cand_round.time_left_sec
                                    >= e.timing_floor_sec(self.cfg.scanner.min_time_left_sec)
                        });
                if !waived {
                    let mid = self
                        .book_snapshot(&sig.token_id)
                        .map(|b| b.mid_price)
                        .unwrap_or(Decimal::ZERO);
                    self.near_miss.on_blocked(
                        &sig.token_id,
                        &sig.asset,
                        sig.direction.as_str(),
                        sig.price,
                        mid,
                        crate::shadow::NearMissReason::Timing,
                        cand_round.time_left_sec,
                        cand_round.slot,
                        now_ms,
                    );
                    self.last_blocked.push(BlockedCandidate {
                        strategy: sig.strategy,
                        account_id: sig.account_id,
                        token_id: sig.token_id,
                        asset: sig.asset,
                        direction: sig.direction,
                        price: sig.price,
                        reason: BlockReason::Timing,
                        time_left_sec: cand_round.time_left_sec,
                    });
                    continue;
                }
                self.last_exemptions.push(GateExemptionRecord {
                    strategy: sig.strategy.clone(),
                    account_id: sig.account_id.clone(),
                    gate: "timing",
                    token_id: sig.token_id.clone(),
                    asset: sig.asset.clone(),
                    detail: block.to_string(),
                    time_left_sec: cand_round.time_left_sec,
                });
            }
            if let Err(detail) = self.momentum_ok(&sig.asset, sig.direction, now_ms) {
                let waived = self
                    .strategy_gate_exemptions(&sig.strategy)
                    .is_some_and(|e| e.momentum);
                if !waived {
                    let mid = self
                        .book_snapshot(&sig.token_id)
                        .map(|b| b.mid_price)
                        .unwrap_or(Decimal::ZERO);
                    self.near_miss.on_blocked(
                        &sig.token_id,
                        &sig.asset,
                        sig.direction.as_str(),
                        sig.price,
                        mid,
                        crate::shadow::NearMissReason::Momentum,
                        cand_round.time_left_sec,
                        cand_round.slot,
                        now_ms,
                    );
                    self.last_blocked.push(BlockedCandidate {
                        strategy: sig.strategy,
                        account_id: sig.account_id,
                        token_id: sig.token_id,
                        asset: sig.asset,
                        direction: sig.direction,
                        price: sig.price,
                        reason: BlockReason::Momentum,
                        time_left_sec: cand_round.time_left_sec,
                    });
                    continue;
                }
                self.last_exemptions.push(GateExemptionRecord {
                    strategy: sig.strategy.clone(),
                    account_id: sig.account_id.clone(),
                    gate: "momentum",
                    token_id: sig.token_id.clone(),
                    asset: sig.asset.clone(),
                    detail,
                    time_left_sec: cand_round.time_left_sec,
                });
            }

            // A strategy-declared share count (pair legs must match in SHARES,
            // not in notional) is honoured only inside the kernel's risk band:
            // capped at the same `max_shares` ceiling the notional path obeys,
            // so a declaration can never oversize. When the equity-relative
            // mode is armed (#202), the declared count is ALSO capped by the
            // strategy's whole-share equity ticket — a percentage budget is an
            // upper bound on committed cash, and a depth-based declaration is
            // exactly the path that would otherwise spend past it. The cap is
            // uniform across a signal batch, so a pair's two legs (declared
            // equal, clamped by the same budget at near-equal prices) stay
            // mergeable. A declared count below one share buys NOTHING —
            // falling back to notional sizing would open the naked leg of a
            // pair — and `None` keeps notional sizing.
            let size = match sig.shares {
                Some(n) => {
                    if n < Decimal::ONE {
                        continue;
                    }
                    let sizing = self.effective_sizing(&sig.strategy);
                    let mut capped = n.min(sizing.max_shares);
                    if sizing.size_pct > Decimal::ZERO {
                        let ticket = self.equity_shares(sig.price, &sizing, &sig.account_id);
                        if ticket <= Decimal::ZERO {
                            self.size_pct_skipped += 1;
                            continue;
                        }
                        capped = capped.min(ticket);
                    }
                    capped
                }
                // #202: an equity-relative budget too small for one whole share
                // produces NO order (counted, not emitted as a 0-size rejection).
                None => {
                    let Some(size) = self.entry_ticket(sig.price, &sig.strategy, &sig.account_id)
                    else {
                        self.size_pct_skipped += 1;
                        continue;
                    };
                    size
                }
            };
            orders.push(crate::model::OrderRequest {
                token_id: sig.token_id.clone(),
                condition_id: sig.condition_id,
                side: crate::model::Side::Buy,
                mode: crate::model::FillPolicy::MakerThenTaker,
                price: sig.price,
                size,
                internal_key: if sig.account_id == blitzkrieg_market_api::DEFAULT_ACCOUNT_ID {
                    // Default scope: byte-identical to the pre-fanout key —
                    // the replay baseline compares these strings. E3: the
                    // slot segment is the CANDIDATE's own family slot (the
                    // single-family corpora produce the same numbers).
                    format!(
                        "{}:{}:{}:{}",
                        sig.strategy,
                        sig.asset,
                        sig.direction.as_str(),
                        cand_round.slot
                    )
                } else {
                    // A second scope on the same strategy/asset/slot must not
                    // collide with the default scope's key in the OME's
                    // global dedup: the account segment disambiguates.
                    format!(
                        "{}:{}:{}:{}:{}",
                        sig.strategy,
                        sig.asset,
                        sig.direction.as_str(),
                        cand_round.slot,
                        sig.account_id
                    )
                },
                strategy: sig.strategy,
                asset: sig.asset.clone(),
                direction: sig.direction.as_str().to_string(),
                round_slot: cand_round.slot,
                // E28 §9.2, extended by the multi-account fanout: the signal
                // carries its SCOPE's account (the default scope keeps the
                // process default, byte-identical to the old behaviour).
                account_id: blitzkrieg_market_api::AccountId(sig.account_id.clone()),
            });
        }
        orders
    }

    /// Drain finalized near-miss records for persistence.
    pub fn take_near_misses(&mut self, now_ms: i64) -> Vec<crate::shadow::NearMissRecord> {
        self.near_miss.tick(now_ms)
    }

    /// #427: the venue the token's market listed on, read off the current
    /// round's market list — pure DATA for the panels' Venue column (a
    /// cross-venue pair shows one leg per venue). `None` when the engine is
    /// down or the token belongs to no live market; the caller renders that
    /// as the undeclared case.
    pub fn venue_of(&self, token_id: &str) -> Option<String> {
        self.scanner
            .markets()
            .iter()
            .find(|m| m.up_token_id == token_id || m.down_token_id == token_id)
            .map(|m| m.venue.clone())
            .filter(|v| !v.is_empty())
    }

    /// The venue that should EXECUTE `token` for `account`. The table row's
    /// venue, with one override: an account bound to a venue whose STATIC
    /// mirror declaration names the row's venue (the constructive fact — a
    /// parasitic mirror plugin never declares rounds; PolySimulator's token
    /// ids ARE Polymarket's by construction). The executor drain's routing
    /// truth: per-venue drains key on this, so a mirror token's two accounts
    /// each reach their OWN executor and the poly account's live order can
    /// never land on the real venue (nor the reverse). Unbound accounts and
    /// non-mirror tokens ride the plain table lookup, byte-identical with
    /// the pre-mirror routing. The display book (`shadow_markets`) is NOT
    /// routing evidence (#643): runtime observation wobbles with the feed,
    /// the declaration does not.
    pub fn execution_venue_for(&self, account: &str, token_id: &str) -> Option<String> {
        let table = self.venue_of(token_id);
        match (
            table,
            self.venue_by_account.get(account).filter(|v| !v.is_empty()),
        ) {
            (Some(v), Some(b)) if v != *b => {
                if self.mirror_source_of(b) == Some(v.as_str()) {
                    Some(b.clone())
                } else {
                    Some(v.clone())
                }
            }
            (other, _) => other,
        }
    }

    /// The condition's OTHER outcome token, read off the current round's
    /// market list — the dry settlement ladder's counter-evidence key. The
    /// binary market structure guarantees a held token's partner exists while
    /// the market is live; `None` when the token belongs to no live market
    /// (the caller stamps empty and the ladder keeps its withhold behavior).
    pub fn partner_of(&self, token_id: &str) -> Option<String> {
        self.scanner.markets().iter().find_map(|m| {
            if m.up_token_id == token_id {
                Some(m.down_token_id.clone())
            } else if m.down_token_id == token_id {
                Some(m.up_token_id.clone())
            } else {
                None
            }
        })
    }

    /// #461: whether the token's market settles through the NegRisk adapter —
    /// the merge call's routing key. `None`/`false` merge through the plain
    /// CTF; a neg-risk market would revert there, so the request must say so.
    pub fn neg_risk_of(&self, token_id: &str) -> Option<bool> {
        self.scanner
            .markets()
            .iter()
            .find(|m| m.up_token_id == token_id || m.down_token_id == token_id)
            .map(|m| m.neg_risk)
    }

    /// The token's live round slot (the market that currently lists it).
    /// The pair-chase request's round identity: a NEXT round's orphan gets a
    /// fresh internal key, so a retried chase never collides with the OME's
    /// duplicate-intent guard. `None` = the token belongs to no live market.
    pub fn round_slot_of(&self, token_id: &str) -> Option<i64> {
        self.scanner
            .markets()
            .iter()
            .find(|m| m.up_token_id == token_id || m.down_token_id == token_id)
            .map(|m| m.round_slot)
    }

    /// The token's market asset (the request's provenance echo).
    pub fn asset_of(&self, token_id: &str) -> Option<String> {
        self.scanner
            .markets()
            .iter()
            .find(|m| m.up_token_id == token_id || m.down_token_id == token_id)
            .map(|m| m.asset.clone())
    }

    /// The token's leg label (`"up"` / `"down"`, read off its market side —
    /// the same wire direction the engine stamps on entry requests).
    pub fn direction_of(&self, token_id: &str) -> Option<SignalDirection> {
        self.scanner.markets().iter().find_map(|m| {
            if m.up_token_id == token_id {
                Some(SignalDirection::Up)
            } else if m.down_token_id == token_id {
                Some(SignalDirection::Down)
            } else {
                None
            }
        })
    }

    /// Force-finalize every pending near-miss (used on shutdown so records are
    /// not lost when the process stops before a window elapses).
    pub fn flush_near_misses(&mut self) -> Vec<crate::shadow::NearMissRecord> {
        self.near_miss.flush()
    }

    /// Near-miss signals blocked on the most recent evaluation.
    pub fn last_blocked(&self) -> &[BlockedCandidate] {
        &self.last_blocked
    }
    pub fn blocked_timing_count(&self) -> u64 {
        self.blocked_timing_total
    }
    pub fn blocked_momentum_count(&self) -> u64 {
        self.blocked_momentum_total
    }

    /// The per-strategy gate exemptions registered strategies declared
    /// (E2-b / #27). None for an unknown strategy.
    /// E25 (#331): the token ids tradable THIS round (both sides of every
    /// market) — Gate 1's "token 属于本轮" check reads this.
    pub fn round_token_ids(&self) -> Vec<String> {
        self.scanner
            .markets()
            .iter()
            .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
            .collect()
    }

    /// E25 (#331): `time_left_sec` of the most recent evaluate cycle.
    pub fn last_time_left_sec(&self) -> i64 {
        self.last_time_left_sec
    }

    pub fn strategy_gate_exemptions(&self, name: &str) -> Option<GateExemptions> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.strategy.gate_exemptions())
    }

    /// Replay fast path: the earliest virtual instant the round-timing
    /// verdict (`scanner.can_trade_reason`) can flip for ANY registered
    /// strategy, computed from the round clock's integer-second fields —
    /// the min-round-age start and the min-time-left floor, each pulled
    /// earlier by 1 s so the wake tick lands at or before the boundary a
    /// full evaluation would have seen. Strategies with a timing exemption
    /// contribute their own (lower) floor. `None` when no market is loaded
    /// (the next rollover re-arms the clock via `fast_next_timing_ms = 0`).
    ///
    /// A tick that evaluates before this instant provably reproduces the
    /// previous evaluation: the round clock's second-hand fields are the
    /// only strategy inputs that move with `now_ms` between events, and
    /// their comparisons cannot change verdict inside the open interval.
    pub fn next_timing_boundary_ms(&self, now_ms: i64) -> Option<i64> {
        if self.scanner.markets().is_empty() {
            return None;
        }
        // E3: the table's own family clock (single-family tables resolve the
        // duration to the configured grid — the same numbers the global read
        // produced). NOT one of the audited strategy-visible sites: this is
        // the replay fast path's WAKE predictor, and its conservative
        // whole-table expiry stays the correct wake bound for every family.
        let round = self.table_round_timing(now_ms);
        let expires = round.expires_at_ms;
        let t = round.time_left_sec.max(0);
        let mut best: i64 = i64::MAX;
        // A timing verdict flips when time_left_sec first reads a boundary
        // value. time_left reads V-1 first at `expires - V*1000 + 1` and
        // reads V first at `expires - V*1000 - 999` (integer-second windows).
        // A boundary at/behind now clamps to now+1: the next tick re-checks
        // (cheap, and self-correcting if the arithmetic rounds early).
        let consider = |b: i64, best: &mut i64| {
            if b <= now_ms {
                return;
            }
            if b < *best {
                *best = b;
            }
        };
        // Per-strategy time-left floor: allowed while time_left >= floor,
        // flips when it first reads floor-1.
        for s in &self.strategies {
            let e = s.strategy.gate_exemptions();
            let floor = if e.timing {
                e.timing_floor_sec(self.cfg.scanner.min_time_left_sec)
            } else {
                self.cfg.scanner.min_time_left_sec
            };
            if floor > 0 && t >= floor {
                consider(expires - floor * 1000 + 1, &mut best);
            }
        }
        // Min-round-age: blocked while age < min_age; flips when time_left
        // first reads (duration - min_age). E3: the duration is the table's
        // resolved family's (single-family = the configured grid, verbatim).
        let min_age = self.cfg.scanner.min_round_age_sec.max(0);
        if round.age_sec < min_age {
            let v = round.age_sec - min_age + round.time_left_sec;
            if v > 0 {
                consider(expires - v * 1000 - 999, &mut best);
            }
        }
        if best == i64::MAX {
            None
        } else {
            Some(best.max(now_ms + 1))
        }
    }

    /// Every declared exemption, as `(strategy, exemptions)` in registration
    /// order — the audit view of who opted out of what.
    pub fn declared_gate_exemptions(&self) -> Vec<(String, GateExemptions)> {
        self.strategies
            .iter()
            .map(|s| (s.strategy.name().to_string(), s.strategy.gate_exemptions()))
            .filter(|(_, e)| e.any())
            .collect()
    }

    /// Whether the named strategy declared hold-to-settlement semantics: its
    /// positions are meant to be redeemed at expiry, not sold on the exit
    /// ladder (a complete-set pair pays $1 at settlement regardless of which
    /// side wins — selling a leg before expiry would destroy that payoff).
    /// Read by the host's exit checks; `false` for an unknown strategy.
    pub fn strategy_holds_to_settlement(&self, name: &str) -> bool {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.holds_to_settlement())
    }

    /// E-VA: whether the named strategy declared ATOMIC-BATCH semantics — its
    /// candidates in one evaluation cycle reserve the virtual quota as ONE
    /// batch before the first leg goes out. `false` for an unknown strategy
    /// and for every strategy that did not declare it (the per-order path).
    pub fn strategy_atomic_batch(&self, name: &str) -> bool {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.atomic_batch())
    }

    /// True when EVERY ENABLED strategy holds to settlement — the replay
    /// fast path's precondition for skipping the exit ladder between dirty
    /// ticks (a ladder verdict against a holder is discarded by the host
    /// anyway, so re-running it per maintenance tick with a frozen book is
    /// pure waste). False when any enabled non-holder is registered or the
    /// engine is absent, so mixed books keep the full ladder.
    ///
    /// Disabled strategies are excluded deliberately: a disabled strategy
    /// contributes nothing (no evaluation, no orders, no exits — its hook
    /// output is dropped, see `drain_breaks`), so during a replay it can
    /// never hold a position and its non-holder declaration must not veto
    /// the fast path. A registry of nine libraries with one enabled holder
    /// is exactly the deployment shape the fast path is for.
    pub fn all_strategies_hold_to_settlement(&self) -> bool {
        self.strategies
            .iter()
            .filter(|s| s.enabled)
            .all(|s| s.strategy.holds_to_settlement())
    }

    /// Gate exemptions honoured on the most recent evaluation, in the order the
    /// candidates were evaluated. Cleared at the start of every `evaluate`.
    pub fn last_exemptions(&self) -> &[GateExemptionRecord] {
        &self.last_exemptions
    }

    /// Drain the honoured-exemption records (called by the host after evaluate)
    /// and fold them into the per-strategy tallies (per-name AND per-account).
    pub fn take_exemptions(&mut self) -> Vec<GateExemptionRecord> {
        let out = std::mem::take(&mut self.last_exemptions);
        for r in &out {
            let t = self.gate_tally.entry(r.strategy.clone()).or_default();
            match r.gate {
                "timing" => t.exempted_timing += 1,
                _ => t.exempted_momentum += 1,
            }
            let key = format!("{}\u{1}{}", r.strategy, r.account_id);
            let t = self.gate_tally_acc.entry(key).or_default();
            match r.gate {
                "timing" => t.exempted_timing += 1,
                _ => t.exempted_momentum += 1,
            }
        }
        out
    }

    /// Blocked / exempted counts for one strategy (E2-b / #27).
    pub fn gate_tally(&self, name: &str) -> StrategyGateTally {
        self.gate_tally.get(name).copied().unwrap_or_default()
    }

    /// Blocked / exempted counts for one (strategy, account) scope — the
    /// account slice the per-account strategy page reads. Zeros when the
    /// scope never saw a gate verdict.
    pub fn gate_tally_for_account(&self, name: &str, account: &str) -> StrategyGateTally {
        let key = format!("{}\u{1}{}", name, account);
        self.gate_tally_acc.get(&key).copied().unwrap_or_default()
    }

    /// Accumulate blocked counters (called by the caller after evaluate).
    pub fn tally_blocked(&mut self) {
        for b in &self.last_blocked {
            match b.reason {
                BlockReason::Timing => self.blocked_timing_total += 1,
                BlockReason::Momentum => self.blocked_momentum_total += 1,
            }
            let t = self.gate_tally.entry(b.strategy.clone()).or_default();
            match b.reason {
                BlockReason::Timing => t.blocked_timing += 1,
                BlockReason::Momentum => t.blocked_momentum += 1,
            }
            let key = format!("{}\u{1}{}", b.strategy, b.account_id);
            let t = self.gate_tally_acc.entry(key).or_default();
            match b.reason {
                BlockReason::Timing => t.blocked_timing += 1,
                BlockReason::Momentum => t.blocked_momentum += 1,
            }
        }
    }

    /// Attach (or detach) the Shadow Evolution per-strategy parameter registry.
    /// Called when evolution is (re)configured; idempotent. Forwarded to every
    /// registered strategy, which resolves its OWN cell from it (E2-c / #28).
    ///
    /// `None` DETACHES: strategies fall back to the config pushed via
    /// `on_config`, which is what makes "evolution disabled" provably identical
    /// to the pre-feature behaviour rather than merely inert by convention.
    pub fn set_hot_params(
        &mut self,
        registry: Option<std::sync::Arc<crate::shadow_evolution::ParamRegistry>>,
    ) {
        self.hot_params = registry.clone();
        for s in &mut self.strategies {
            s.strategy.set_hot_params(registry.clone());
        }
    }

    pub fn has_hot_params(&self) -> bool {
        self.hot_params.is_some()
    }

    /// Per-strategy config-in-force views as (name, JSON string) for every
    /// strategy that reports one (observability). An external library reports
    /// through its OPTIONAL `bk_strategy_config_view` symbol; strategies that
    /// declare nothing are simply absent. The kernel keeps no knowledge of any
    /// strategy's parameter names.
    pub fn strategy_config_views(&self) -> Vec<(String, String)> {
        self.strategies
            .iter()
            .filter_map(|s| {
                s.strategy
                    .config_view_json()
                    .map(|v| (s.strategy.name().to_string(), v))
            })
            .collect()
    }

    /// Strategy names the engine knows about, in registration order (PR-B: the
    /// kernel ships none, so this is exactly what the loader registered).
    pub fn supported_strategies(&self) -> Vec<String> {
        self.strategies
            .iter()
            .map(|s| s.strategy.name().to_string())
            .collect()
    }
    /// Borrow every hosted strategy. Shadow Evolution asks each one which knobs it
    /// declares (E2-c), so the declaration has to be read off the live instances.
    ///
    /// Deliberately NOT filtered by `enabled`: `register_strategies` drops the cell
    /// of any strategy it is not handed, so filtering here would make a runtime
    /// disable destroy that strategy's evolved parameters and rollback anchor.
    /// A switched-off strategy just never emits the candidates that would use them.
    pub fn strategy_refs(&self) -> Vec<&dyn EngineStrategy> {
        self.strategies
            .iter()
            .map(|s| s.strategy.as_ref())
            .collect()
    }
    /// Enabled strategy names.
    pub fn enabled_strategies(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .strategies
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.strategy.name().to_string())
            .collect();
        v.sort();
        v
    }

    /// RFC 568 阶段一：borrow every hosted scope (name/enabled/account per
    /// instance) —— Core 侧构建 per-owner 的配对簿视图快照用（views_for 的
    /// owner 过滤要实例主账户 + arb 腿账户白名单，两者都在 HostedStrategy
    /// 上）。crate 内可见：host 内部接线，非公开 API 面。
    pub(crate) fn hosted(&self) -> &[HostedStrategy] {
        &self.strategies
    }
    /// Toggle a strategy; returns false if the name is unknown to the engine.
    ///
    /// Multi-scope semantics (账户↔venue 解耦裁决): the toggle is the
    /// GLOBAL strategy switch — the master power for every scope instance of
    /// `name`, default scopes included (a global OFF stops the strategy
    /// everywhere; a global ON hands the decision back to each account's
    /// enablement set). Per-account selectivity is the account book's
    /// `strategies` ENABLEMENT set (which per-account scopes run), not this
    /// switch: an account that has the strategy in its set keeps running it
    /// while the global switch is ON, regardless of other accounts' sets.
    ///
    /// PERSISTENCE ASYMMETRY (issue 552 audit, honest wording): a global OFF
    /// is a session-level fact on the default scope's `enabled` flag — it is
    /// NOT persisted anywhere, so a kernel restart hands the decision back to
    /// the persisted enablement sets again (a fresh boot replays
    /// accounts.toml, and a scope named in an account's set boots ENABLED —
    /// see the scope-rebuild rule). A global ON is likewise session-level.
    /// The DURABLE per-account truth is only the enablement set: to keep a
    /// strategy off across restarts, remove it from every account's set
    /// (the panel's per-account 启用按钮 / `account.strategies.set`).
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let mut found = false;
        for s in &mut self.strategies {
            if s.strategy.name() == name {
                s.enabled = enabled;
                found = true;
            }
        }
        found
    }

    /// Register a strategy into the live dispatch.
    ///
    /// This is the ONLY way a strategy enters the engine. The kernel ships none
    /// (PR-B): external v2 dylibs come through the C ABI v2 loader, which calls
    /// this. It starts DISABLED: an explicit enable is required before it can
    /// place orders. Returns the registered name, or an error when the name is
    /// taken.
    pub fn register_user_strategy(
        &mut self,
        strategy: Box<dyn EngineStrategy>,
        source: String,
    ) -> Result<String, String> {
        self.register_scoped_strategy(strategy, source, blitzkrieg_market_api::DEFAULT_ACCOUNT_ID)
    }

    /// Register a strategy instance into ONE account scope. The (account,
    /// name) pair must be unique — two scopes may both host `spread_arb`,
    /// each with its own Lua state, sizing and book; one scope cannot host
    /// the same strategy twice. Starts DISABLED like the default-scope form.
    pub fn register_scoped_strategy(
        &mut self,
        strategy: Box<dyn EngineStrategy>,
        source: String,
        account_id: &str,
    ) -> Result<String, String> {
        let name = strategy.name().to_string();
        if self
            .strategies
            .iter()
            .any(|s| s.strategy.name() == name && s.account_id == account_id)
        {
            return Err(format!(
                "strategy name already registered for account {account_id}: {name}"
            ));
        }
        // Provenance is sticky: the FIRST registration's source wins, so a
        // scope teardown can never orphan a re-assignment.
        self.package_sources
            .entry(name.clone())
            .or_insert_with(|| source.clone());
        let mut hosted = HostedStrategy {
            strategy,
            enabled: false,
            source,
            account_id: account_id.to_string(),
            exits_only: false,
        };
        // Hand the host config to the newly registered strategy.
        hosted
            .strategy
            .on_config(&self.cfg.trend, &self.cfg.spread_arb);
        // Registered after Shadow Evolution was configured? Forward the handle
        // so a later evolution still reaches this strategy.
        if let Some(h) = &self.hot_params {
            hosted.strategy.set_hot_params(Some(h.clone()));
        }
        // 跨所套利弹窗 v2: a scope registered while an arb configuration is
        // already in force receives ITS PROJECTION at birth (the same
        // "later joiner" pattern the hot_params forward runs) — otherwise a
        // scope added after an `arb.config.set` would stamp instance
        // accounts until the next set. `set_arb_leg_config` owns the
        // per-scope projection (the wired scope gets the map, a separate
        // deployment keeps its own book).
        let joiner_cfg = self.arb_leg_config.clone();
        let joiner_account = account_id.to_string();
        let scope_cfg = match (&joiner_cfg, joiner_account.as_str()) {
            (Some(c), acct) if acct == c.leg_a_account.trim() || acct == c.leg_b_account.trim() => {
                Some(c.clone())
            }
            (Some(c), acct) => {
                let mut projected = c.clone();
                projected.leg_a_account = acct.to_string();
                projected.leg_b_account = acct.to_string();
                Some(projected)
            }
            (None, _) => None,
        };
        hosted.strategy.set_arb_leg_config(scope_cfg);
        self.strategies.push(hosted);
        Ok(name)
    }

    /// Where a registered strategy came from (`dylib:<path>`, `test`, ...) —
    /// free-form provenance the loader supplies. The kernel itself has no
    /// `builtin` class any more.
    pub fn strategy_source(&self, name: &str) -> Option<&str> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.source.as_str())
    }

    /// Remove a strategy from the live dispatch (E9-b `strategy.unload` /
    /// `strategy.reload`).
    ///
    /// A registered strategy is DYNAMIC by construction — the kernel ships
    /// none — so every registered name is removable. The guard refuses while the
    /// strategy is ENABLED: an operator must disable it first, so a
    /// still-trading strategy is never silently pulled from under a running
    /// session. Returns `Err` with the reason on any refusal, `Ok(true)` when
    /// removed, `Ok(false)` when the name is unknown. An UNLOAD also drops
    /// the sticky provenance — the package is gone from the process, the
    /// name must stop being assignable (an assignment rebuild would find no
    /// source and warn-skip).
    pub fn unregister_user_strategy(&mut self, name: &str) -> Result<bool, String> {
        let Some(idx) = self
            .strategies
            .iter()
            .position(|s| s.strategy.name() == name)
        else {
            return Ok(false);
        };
        if self.strategies[idx].enabled {
            return Err(format!(
                "strategy {name} is ENABLED — disable it first (strategy.enable {name} false)"
            ));
        }
        self.strategies.remove(idx);
        self.package_sources.remove(name);
        Ok(true)
    }

    /// Remove EVERY scope instance of `name` while keeping the package's
    /// provenance (this is the ASSIGNMENT teardown — the package is still
    /// loaded in the process and a future assignment must be able to
    /// re-instantiate it; only `unregister_user_strategy` — the physical
    /// unload — drops the source).
    pub fn unregister_strategy_all_scopes(&mut self, name: &str) -> Result<bool, String> {
        if self
            .strategies
            .iter()
            .any(|s| s.strategy.name() == name && s.enabled)
        {
            return Err(format!(
                "strategy {name} is ENABLED — disable it first (strategy.enable {name} false)"
            ));
        }
        let before = self.strategies.len();
        self.strategies.retain(|s| s.strategy.name() != name);
        Ok(self.strategies.len() != before)
    }

    /// Scope instances of one strategy (account ids, registration order).
    pub fn strategy_scopes(&self, name: &str) -> Vec<String> {
        self.strategies
            .iter()
            .filter(|s| s.strategy.name() == name)
            .map(|s| s.account_id.clone())
            .collect()
    }

    /// issue 561 账户维重置：全部 (account, strategy) scope 对，注册序。
    /// 重置分支据此枚举目标账户的 scopes（停用其已启用者），不需要预先
    /// 知道策略名 —— 全量对先拿到再说。
    pub fn strategy_scopes_all(&self) -> Vec<(String, String)> {
        self.strategies
            .iter()
            .map(|s| (s.account_id.clone(), s.strategy.name().to_string()))
            .collect()
    }

    /// The (account, strategy) pairs currently demoted exits-only — the
    /// held-position safety net's live state, for diagnostics and tests.
    pub fn exits_only_scopes(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .strategies
            .iter()
            .filter(|s| s.exits_only)
            .map(|s| (s.account_id.clone(), s.strategy.name().to_string()))
            .collect();
        v.sort();
        v
    }

    /// Is `name` assignable? Every package LOADED this session (live scopes
    /// included, torn-down scopes' sources retained). The assignable-set
    /// check `account.strategies.set` runs plan-first against this.
    pub fn strategy_registered(&self, name: &str) -> bool {
        self.package_sources.contains_key(name)
    }

    /// (strategy name → source string) for every package LOADED this
    /// session, first registration wins. The host reads this BEFORE opening
    /// its engine borrow so the scope-builder closure can re-instantiate
    /// packages without touching engine state. Sources survive scope
    /// teardowns — a dropped scope's strategy stays re-assignable.
    pub fn scope_sources(&self) -> Vec<(String, String)> {
        self.package_sources
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect()
    }

    /// Rebuild one account's scope set from its strategy ENABLEMENT set
    /// (账户↔venue 解耦语义: `requested` is the account's 启用集 — the
    /// persisted 启用按钮 state — not an assignment or a visibility filter).
    /// Three phases, in this order:
    ///
    /// 1. DEMOTE: a scope of this account whose strategy is not in
    ///    `requested` but which still HELD open positions (`held`) flips to
    ///    exits-only — it contributes no entry candidates from the next
    ///    cycle on, its exit intents keep flowing until the account is flat.
    /// 2. DROP: a scope whose strategy is out of the set and flat (not in
    ///    `held`) is removed outright — its Lua state dies with it.
    /// 3. ADD: every requested strategy without a scope for this account is
    ///    instantiated through `build` (the host's Lua re-loader; `None`
    ///    means the source is not rebuildable — a dylib or vanished dir —
    ///    and the name is SKIPPED with a warning, never a panic) and arrives
    ///    ENABLED.
    ///
    /// Returns the names kept as exits-only (phase 1), for the reply.
    pub fn rebuild_account_scopes(
        &mut self,
        account: &str,
        requested: &[String],
        held: &[String],
        build: &mut ScopeBuilder<'_>,
    ) -> Vec<String> {
        let mut kept_exits_only: Vec<String> = Vec::new();
        // 1. Demote removed-and-held.
        for s in &mut self.strategies {
            if s.account_id != account {
                continue;
            }
            let name = s.strategy.name().to_string();
            if !requested.contains(&name) && held.contains(&name) {
                if !s.exits_only {
                    s.exits_only = true;
                }
                kept_exits_only.push(name);
            }
        }
        // 2. Drop removed-and-flat.
        //
        // The enablement write is a DECISION: a scope the request omits and
        // no position holds is torn down outright. Durable exposure is
        // covered by the DEMOTE phase above — a held strategy keeps managing
        // its exits until the account is flat.
        self.strategies.retain(|s| {
            if s.account_id != account {
                return true;
            }
            let name = s.strategy.name();
            requested.iter().any(|r| r == name) || held.iter().any(|h| h == name)
        });
        // 3. Add newly-requested scopes.
        for name in requested {
            match self
                .strategies
                .iter_mut()
                .find(|s| s.account_id == account && s.strategy.name() == *name)
            {
                Some(existing) => {
                    // A scope BACK in the enablement set returns to normal:
                    // the demotion clears AND the scope enables — being named
                    // in the set IS the 启用 state (the same rule the freshly
                    // built scope below gets). A pre-existing scope the boot
                    // registered DISABLED (no state-file replay, a fresh
                    // package) therefore turns on exactly when the file's
                    // enablement set names it.
                    existing.exits_only = false;
                    existing.enabled = true;
                }
                None => {
                    let Some(built) = build(name, account) else {
                        tracing::warn!(
                            strategy = %name,
                            account = %account,
                            "scope rebuild: source not rebuildable — scope skipped"
                        );
                        continue;
                    };
                    if let Err(reason) = self.register_scoped_strategy(
                        built,
                        format!("lua:scope:{account}"),
                        account,
                    ) {
                        tracing::warn!(
                            strategy = %name,
                            account = %account,
                            %reason,
                            "scope rebuild: scope registration refused"
                        );
                        continue;
                    }
                    // Being named in an account's enablement set is the
                    // operator's intent to run it there, so a rebuilt scope
                    // arrives ENABLED. The old rule inherited the default
                    // scope's global flag — an account given a
                    // globally-disabled strategy then silently never traded,
                    // with no error anywhere (生产 probe2 实锤: market_maker
                    // /mad_dog assigned but dead while spread_arb ran).
                    for s in &mut self.strategies {
                        if s.account_id == account && s.strategy.name() == *name {
                            s.enabled = true;
                        }
                    }
                }
            }
        }
        kept_exits_only
    }

    /// Default-scope convenience: the service layer calls
    /// `note_scoped_order_placed`; tests and legacy callers use this.
    pub fn note_order_placed(&mut self, token_id: &str) {
        self.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, token_id);
    }

    /// Mark a (scope, token) pair as having a live entry order this round
    /// (suppresses repeats).
    pub fn note_scoped_order_placed(&mut self, account: &str, token_id: &str) {
        self.pending_tokens
            .insert((account.to_string(), token_id.to_string()));
    }

    /// Release the repeat-suppression on a token whose entry order reached a
    /// terminal state (filled, cancelled, rejected). The suppression exists to
    /// stop duplicate RESTING orders, not to pin the round: a partially-filled
    /// leg that ended its order is exactly the gap a strategy may need to top
    /// up this same round. No-op for a token that never had an order.
    pub fn release_pending_token(&mut self, token_id: &str) {
        self.pending_tokens.remove(&(
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            token_id.to_string(),
        ));
    }

    /// Scope-aware release used by the service layer: the order carried an
    /// account_id, so the release targets exactly that scope's suppression.
    pub fn release_scoped_pending_token(&mut self, account: &str, token_id: &str) {
        self.pending_tokens
            .remove(&(account.to_string(), token_id.to_string()));
    }

    /// The release above, scoped to the strategy that placed the order: a
    /// hold-to-settlement strategy (the pair packages) needs the gate reopened
    /// when its leg goes terminal — its gap-fill loop is the legitimate
    /// re-entry. Every other strategy may EXIT mid-round and re-enter, and the
    /// historic one-entry-per-token-per-round discipline is what keeps that
    /// re-entry from churning the book: spread_arb measured WORSE on the
    /// frozen exit-economics windows the moment terminal releases applied to
    /// it (12→13, 6→9, 13→15 closes, all three nets down). The unscoped
    /// release stays public for the TEST-ONLY callers that assert the
    /// mechanism. Returns whether the suppression was actually lifted.
    pub fn release_pending_token_for_strategy(
        &mut self,
        account: &str,
        strategy: &str,
        token_id: &str,
    ) -> bool {
        let is_holder = self.strategies.iter().any(|s| {
            s.strategy.name() == strategy
                && s.account_id == account
                && s.strategy.holds_to_settlement()
        });
        if is_holder {
            self.pending_tokens
                .remove(&(account.to_string(), token_id.to_string()))
        } else {
            false
        }
    }

    /// Drain strategy close intents gathered during the latest evaluate(). The
    /// host routes these into the same exit-submission path as policy exits.
    pub fn drain_strategy_exits(&mut self) -> Vec<StrategyExitIntent> {
        std::mem::take(&mut self.strategy_exits)
    }

    /// E30 (§6.3): drain the one-shot poison alerts from sandboxed strategies.
    /// Each name pairs with the single alert message the core raises as one
    /// RISK_ALERT; the sandbox itself refuses every later call from a poisoned
    /// strategy, so this is reporting, not enforcement.
    pub fn drain_poison_alerts(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for s in self.strategies.iter_mut() {
            if let Some(alert) = s.strategy.poison_alert() {
                out.push((s.strategy.name().to_string(), alert));
            }
        }
        out
    }

    fn compute_shares(&self, price: Decimal, strategy: &str, account: &str) -> Decimal {
        let sizing = self.effective_sizing(strategy);
        // P0 #202: the equity-relative mode REPLACES the absolute budget when
        // it is armed. Everything below this line is the historical path, and
        // with `size_pct = 0` (the default) it is reached unchanged.
        if sizing.size_pct > Decimal::ZERO {
            return self.equity_shares(price, &sizing, account);
        }
        // A zero budget zeroes the leg outright (an explicit weight-0 off
        // switch): the share floor exists to protect a real entry, not to
        // resurrect a disabled one.
        if sizing.size_usd <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        if price <= Decimal::ZERO {
            return sizing.min_shares;
        }
        let raw = (sizing.size_usd / price).round();
        raw.max(sizing.min_shares).min(sizing.max_shares)
    }

    /// The ticket for one signal, or `None` when the configured sizing produces
    /// NO order at all (#202). Only the equity-relative mode can answer `None`
    /// — an absolute budget of 0 keeps emitting its historical zero-size order,
    /// so an unconfigured deployment is untouched.
    ///
    /// A relative budget that cannot buy one whole share is a statement about
    /// the account (too small for this percentage), and a 0-size order would
    /// only convert it into a per-tick "size must be positive" rejection; the
    /// skip is counted instead (`size_pct_skip_count`) so it stays visible.
    fn entry_ticket(&self, price: Decimal, strategy: &str, account: &str) -> Option<Decimal> {
        let size = self.compute_shares(price, strategy, account);
        if size > Decimal::ZERO {
            return Some(size);
        }
        if self.effective_sizing(strategy).size_pct > Decimal::ZERO {
            return None;
        }
        Some(size)
    }

    /// The equity-relative ticket (#202).
    ///
    /// `budget = equity × pct% × weight`; the ticket is the WHOLE number of
    /// shares that fits inside it (floor, not round — a percentage budget is an
    /// upper bound, and rounding up would commit up to a share more than the
    /// operator allowed). `max_shares` still caps the lot.
    ///
    /// `min_shares` deliberately does NOT participate here: a share floor that
    /// can raise a ticket over its budget is the #202 bug itself (the default
    /// band [10, 10] is what turned a 4.8 USDC book into 10-share tickets), and
    /// this mode exists to remove it. One whole share is the floor of last
    /// resort; a budget that cannot buy even that emits nothing (see
    /// `entry_ticket`).
    fn equity_shares(&self, price: Decimal, sizing: &EffectiveSizing, account: &str) -> Decimal {
        let equity = self.scope_equity(account);
        if equity <= Decimal::ZERO || price <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let budget = match sizing.size_weight {
            Some(w) => {
                let raw = equity * sizing.size_pct / Decimal::ONE_HUNDRED;
                (raw * w).max(Decimal::ZERO).min(raw)
            }
            None => equity * sizing.size_pct / Decimal::ONE_HUNDRED,
        };
        if budget <= Decimal::ZERO || sizing.max_shares <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (budget / price)
            .floor()
            .max(Decimal::ZERO)
            .min(sizing.max_shares)
    }

    /// The freshness budget in force (#205): the engine refuses to price a book
    /// older than this, and `0` means the check is OFF. Read by the panel's
    /// `engine.stats.orderbookFreshness` so the value that actually gates entries
    /// is reported by the engine that applies it, not re-derived from the config.
    pub fn max_orderbook_stale_ms(&self) -> i64 {
        self.cfg.max_orderbook_stale_ms
    }

    /// The global sizing band: what a strategy without an override gets, and
    /// the CEILING every override is clamped to (E2-a). Also the band a report
    /// reads before any strategy is named (#202's worst-case-order view).
    pub fn global_sizing(&self) -> EffectiveSizing {
        EffectiveSizing {
            size_usd: self.cfg.size_usd,
            min_shares: self.cfg.min_shares,
            max_shares: self.cfg.max_shares,
            size_pct: self.cfg.size_pct,
            strategy_scoped: false,
            size_weight: None,
        }
    }

    /// The sizing in force for one strategy (E2-a): its own override where
    /// configured, otherwise the globals — then clamped so a strategy can never
    /// spend more notional nor hold more shares than the global risk allows.
    /// `min_shares` is also capped by `max_shares` so the reported band is
    /// always coherent (the ceiling wins when an override overshoots).
    pub fn effective_sizing(&self, strategy: &str) -> EffectiveSizing {
        let globals = self.global_sizing();
        let Some(over) = self
            .cfg
            .strategy_sizes
            .get(strategy)
            .filter(|s| s.overrides_anything())
        else {
            return globals;
        };
        let mut size_usd = over
            .size_usd
            .map_or(globals.size_usd, |v| v.min(globals.size_usd));
        if let Some(w) = over.size_weight {
            // Weight re-weights this leg's share of the per-entry budget; it
            // never widens the global cap, and a non-positive weight zeroes the
            // leg out entirely (an explicit off switch).
            size_usd = (size_usd * w).max(Decimal::ZERO).min(globals.size_usd);
        }
        let max_shares = over
            .max_shares
            .map_or(globals.max_shares, |v| v.min(globals.max_shares));
        let min_shares = over
            .min_shares
            .map_or(globals.min_shares, |v| v.max(globals.min_shares))
            .min(max_shares);
        // #202: the global percentage is the ceiling when it is armed. An
        // unarmed (0) global is "no opinion", not a zero ceiling, so a
        // per-strategy percentage is what a single leg gets configured with.
        let size_pct = match over.size_pct {
            Some(v) if globals.size_pct > Decimal::ZERO => v.min(globals.size_pct),
            Some(v) => v.max(Decimal::ZERO),
            None => globals.size_pct,
        };
        EffectiveSizing {
            size_usd,
            min_shares,
            max_shares,
            size_pct,
            strategy_scoped: true,
            size_weight: over.size_weight,
        }
    }
}

/// Book snapshot for pricing, but only while it is fresh enough. Mirrors the
/// engine's historical freshness rule (non-empty book + `max_orderbook_stale_ms`).
/// The freshness check runs off `LocalBook`'s own timestamp, so a stale book
/// never pays for a snapshot allocation at all.
///
/// `max_stale_ms <= 0` is the operator's explicit "freshness check OFF" (#205):
/// the book is then accepted however old it is, and only an EMPTY book still has
/// nothing to price off. Anything else keeps the historic rule, so the default
/// is unchanged.
fn fresh_book(
    books: &HashMap<String, LocalBook>,
    token_id: &str,
    now_ms: i64,
    max_stale_ms: i64,
) -> Option<OrderbookSnapshot> {
    let b = books.get(token_id)?;
    if b.is_empty() || (max_stale_ms > 0 && now_ms - b.timestamp() > max_stale_ms) {
        return None;
    }
    Some(b.snapshot(token_id))
}

/// B1c: a market row's effective round length — its own declared duration when
/// it carries one, else the configured global grid. `0` is the legacy archive's
/// "undeclared" and always resolves to the global duration, so a single-family
/// (all-900s) table and a pre-B1a corpus (all zeros) resolve every row to the
/// same family: the family-scoped merge/rollover verdicts stay byte-identical
/// with the pre-B1c slot-only behaviour. Mirrors `TrackedOrder`'s tri-state
/// resolution (own stamp > global), simplified for rows (no per-row fallback
/// chain beyond the two states the model carries).
///
/// E2: `pub` and shared — the service layer's top-up mirror (the #498
/// staggered-listing membership test) and the kill-arm family scope judge the
/// SAME (slot, duration) pairs the engine's merge arm does, so the two sides
/// cannot drift. Kept a free function (not a `Scanner` method): the callers
/// hold either a `CryptoMarket` or a bare declared duration, never a scanner
/// borrow in the same scope.
pub fn row_family_duration(declared: i64, global_sec: i64) -> i64 {
    if declared > 0 { declared } else { global_sec }
}

impl Engine {
    /// E3 (轮次多族收尾): the round clock of ONE market's own family — the
    /// duration the row declares (`row_family_duration`: own declaration >
    /// global grid), read on the legacy venue bucket (the same fold the
    /// global `round_timing()` used) with only the duration grid refined.
    /// Strategies and the per-candidate gate trail read this family's
    /// slot/time_left, so a 4h market's context no longer reports 15m-grid
    /// numbers. Single-family tables (every replay corpus) resolve every
    /// row to the configured grid, making this byte-identical with the old
    /// global read.
    fn family_round_timing(&self, m: &crate::model::CryptoMarket, now_ms: i64) -> RoundTiming {
        let dur = row_family_duration(m.round_duration_sec, self.cfg.scanner.round_duration_sec);
        self.scanner.round_timing_legacy_at(dur, now_ms)
    }

    /// E3: the round clock of the TABLE's resolved family — every row
    /// through `row_family_duration` must agree (true for every single-family
    /// table: the replay corpora, and the per-family boot instances B1d
    /// mounts); the venue dimension stays the legacy bucket, the same fold
    /// `round_timing()` read. A heterogeneous multi-family table takes the
    /// FIRST row's resolution — a best-effort context for callers with no
    /// candidate to name a market (the per-candidate sites use
    /// [`Self::family_round_timing`]), never a verdict.
    fn table_round_timing(&self, now_ms: i64) -> RoundTiming {
        let global = self.cfg.scanner.round_duration_sec;
        match self.scanner.markets().first() {
            Some(m) => {
                let dur = row_family_duration(m.round_duration_sec, global);
                self.scanner.round_timing_legacy_at(dur, now_ms)
            }
            // No market, no family fact: the legacy global clock (the
            // scanner's `NoMarkets` posture — unchanged).
            None => self.scanner.round_timing(now_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use crate::scanner::TimingBlock;
    use crate::service::{Core, CoreConfig};
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    // ── #498: staggered venue listings must MERGE, not rollover ─────────────

    /// A probe that counts `on_round` fires and reports the scanner's market
    /// count — the exact state a same-slot top-up must and must not touch.
    struct RoundProbe {
        rounds: Arc<Mutex<Vec<i64>>>,
    }
    impl EngineStrategy for RoundProbe {
        fn name(&self) -> &str {
            "round_probe"
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
        fn on_round(&mut self, slot: i64, _left: i64, _now: i64) {
            self.rounds.lock().unwrap().push(slot);
        }
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
            Vec::new()
        }
    }

    fn eth_market(end_ms: i64) -> CryptoMarket {
        CryptoMarket {
            asset: "ETH".into(),
            condition_id: "cond-eth".into(),
            question_id: "q-eth".into(),
            up_token_id: "eth-up".into(),
            down_token_id: "eth-down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "ETH up or down".into(),
        }
    }

    #[test]
    fn a_same_slot_round_feed_merges_without_a_rollover() {
        let rounds = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(RoundProbe {
                rounds: rounds.clone(),
            }),
            "test".into(),
        )
        .unwrap();
        let end = 1_800_000; // slot 2000
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(end)],
            now_ms: 1_000,
        });
        assert_eq!(*rounds.lock().unwrap(), vec![2]);

        // The venue lists the ETH market a minute into the round: a same-slot
        // feed. It must grow the scanner to 2 markets and NOT re-fire
        // on_round (the round never changed).
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(end), eth_market(end)],
            now_ms: 61_000,
        });
        assert_eq!(
            e.scanner.markets().len(),
            2,
            "the late listing joins the round's market list"
        );
        assert_eq!(
            *rounds.lock().unwrap(),
            vec![2],
            "a same-slot top-up is not a round transition"
        );

        // A pure duplicate (both markets already known) is inert.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(end), eth_market(end)],
            now_ms: 62_000,
        });
        assert_eq!(e.scanner.markets().len(), 2);
        assert_eq!(*rounds.lock().unwrap(), vec![2]);
    }

    // ── 盘口批次原子化：Batch 应用路径 ─────────────────────────────────────

    /// BatchProbe 的观测账：`(token, timestamp, best_bid mantissa)`。
    type SeenObs = Vec<(&'static str, i64, i64)>;

    /// 逐事件观测器：记录每次 on_book 调用的 (token, timestamp, best_bid)。
    /// 撕裂探测用它对齐「批次喂入途中」各 token 的可见状态。
    struct BatchProbe {
        seen: Arc<Mutex<SeenObs>>,
    }
    impl EngineStrategy for BatchProbe {
        fn name(&self) -> &str {
            "batch_probe"
        }
        fn on_book(&mut self, t: &str, s: &OrderbookSnapshot, _n: i64) {
            let tok: &'static str = if t == "up" {
                "up"
            } else if t == "down" {
                "down"
            } else {
                "other"
            };
            self.seen
                .lock()
                .unwrap()
                .push((tok, s.timestamp, s.best_bid.mantissa() as i64));
        }
        fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<crate::signal::TradeSignal> {
            Vec::new()
        }
    }

    fn two_leg_market(end_ms: i64) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond-batch".into(),
            question_id: "q-batch".into(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: false,
            question: "?".into(),
        }
    }

    fn batch_engine() -> (Engine, Arc<Mutex<SeenObs>>) {
        let mut e = Engine::new(cfg());
        let seen = Arc::new(Mutex::new(Vec::new()));
        e.register_user_strategy(Box::new(BatchProbe { seen: seen.clone() }), "test".into())
            .expect("probe registers");
        e.set_strategy_enabled("batch_probe", true);
        (e, seen)
    }

    /// 红测试（修复前）：一个 up+down 批次以两个单事件喂入，策略在第二条
    /// on_book 途中读到的 up 是新批次、down 还是上一批 —— 一对不存在的腿间
    /// 折价。修复后 Batch 路径一次应用整批：第二条 on_book 读到的 down 已经
    /// 是同一批（时间戳一致的）快照，两条观测的时间戳必然相同。
    ///
    /// 反过来单事件路径保持原语义：两条逐个喂入的 Book 观测时间戳不同
    /// （它们本来就是两个独立事件），行为逐位不变。
    #[test]
    fn a_batch_of_books_is_observed_at_one_consistent_timestamp() {
        let (mut e, seen) = batch_engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![two_leg_market(1_800_000)],
            now_ms: 1_000,
        });

        // 上一批（老价格，都是 0.40 的 bid）。
        e.on_data(DataEvent::Books {
            books: vec![
                (
                    "up".into(),
                    vec![(dec!(0.40), dec!(10))],
                    vec![(dec!(0.60), dec!(10))],
                ),
                (
                    "down".into(),
                    vec![(dec!(0.40), dec!(10))],
                    vec![(dec!(0.60), dec!(10))],
                ),
            ],
            now_ms: 10_000,
        });
        seen.lock().unwrap().clear();

        // 新批次：up 涨到 0.50，down 涨到 0.55 —— 同一个批次时间戳 20_000。
        e.on_data(DataEvent::Books {
            books: vec![
                (
                    "up".into(),
                    vec![(dec!(0.50), dec!(10))],
                    vec![(dec!(0.60), dec!(10))],
                ),
                (
                    "down".into(),
                    vec![(dec!(0.55), dec!(10))],
                    vec![(dec!(0.60), dec!(10))],
                ),
            ],
            now_ms: 20_000,
        });

        let obs = seen.lock().unwrap().clone();
        assert_eq!(
            obs.len(),
            2,
            "the batch seeds every strategy once per token: {obs:?}"
        );
        assert!(
            obs.iter().all(|(_, ts, _)| *ts == 20_000),
            "every token of one batch must be observed under the SAME batch \
             timestamp — a torn half-old/half-new read is exactly what this \
             forbids: {obs:?}"
        );
        let up = obs
            .iter()
            .find(|(t, _, _)| *t == "up")
            .expect("up observed");
        let down = obs
            .iter()
            .find(|(t, _, _)| *t == "down")
            .expect("down observed");
        assert_eq!(up.2, 50, "the up leg carries its NEW batch price; {obs:?}");
        assert_eq!(
            down.2, 55,
            "the down leg carries its NEW batch price in the same apply; {obs:?}"
        );
    }

    /// 撕裂窗口本体的守卫（变异验证的载体）：把「整批一次应用」退化为
    /// 「逐 token 独立事件应用」后，批次第二条 on_book 途中 up 已是 20_000
    /// 的新价、down 还停在 10_000 的旧价 —— 观测时间戳必然分裂。本测试
    /// 断言这个分裂不存在（绿色），变异后它变红。
    #[test]
    fn a_degenerate_single_token_batch_still_carries_one_timestamp() {
        let (mut e, seen) = batch_engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![two_leg_market(1_800_000)],
            now_ms: 1_000,
        });
        // 同一批 ts 的两个 token 拆成两个 Batch 事件（退化批次）：每次应用
        // 仍各带自己的批次时间戳，一次只动一本 —— 喂入途中不出现跨 token
        // 的一致性假象，也不出现「一个事件两个时间戳」的缝合。
        e.on_data(DataEvent::Books {
            books: vec![(
                "up".into(),
                vec![(dec!(0.50), dec!(10))],
                vec![(dec!(0.60), dec!(10))],
            )],
            now_ms: 20_000,
        });
        e.on_data(DataEvent::Books {
            books: vec![(
                "down".into(),
                vec![(dec!(0.55), dec!(10))],
                vec![(dec!(0.60), dec!(10))],
            )],
            now_ms: 20_000,
        });
        let obs = seen.lock().unwrap().clone();
        assert_eq!(obs.len(), 2);
        assert!(
            obs.iter().all(|(_, ts, _)| *ts == 20_000),
            "each single-token apply stamps its own batch ts: {obs:?}"
        );
    }

    /// 空 Batch 是显式无效输入：不发任何 on_book，也不碰任何状态。
    #[test]
    fn an_empty_batch_is_inert() {
        let (mut e, seen) = batch_engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![two_leg_market(1_800_000)],
            now_ms: 1_000,
        });
        let before = e.books.len();
        e.on_data(DataEvent::Books {
            books: Vec::new(),
            now_ms: 30_000,
        });
        assert!(seen.lock().unwrap().is_empty(), "no observation fired");
        assert_eq!(e.books.len(), before, "no book state touched");
    }

    /// 单事件旧路径守卫：Book 的 timestamp 就是事件自己的 now_ms，逐位不变
    /// —— 回放语料（一个 book 一个事件）的基线语义由这里钉住。
    #[test]
    fn the_single_book_path_keeps_its_event_timestamp() {
        let (mut e, seen) = batch_engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![two_leg_market(1_800_000)],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.42), dec!(10))],
            asks: vec![(dec!(0.58), dec!(10))],
            now_ms: 11_111,
        });
        let obs = seen.lock().unwrap().clone();
        assert_eq!(obs.len(), 1);
        assert_eq!(
            obs[0].1, 11_111,
            "the single-event path stamps the event's own now_ms, byte-identical"
        );
    }

    /// 单事件 TopOfBook 旧路径守卫（同上，回放基线的另一半）。
    #[test]
    fn the_single_top_of_book_path_keeps_its_event_timestamp() {
        let (mut e, seen) = batch_engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![two_leg_market(1_800_000)],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::TopOfBook {
            token_id: "up".into(),
            best_bid: Some(dec!(0.44)),
            best_ask: Some(dec!(0.56)),
            now_ms: 12_222,
        });
        let obs = seen.lock().unwrap().clone();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].1, 12_222);
    }

    #[test]
    fn a_same_slot_top_up_keeps_pending_tokens_and_the_clock() {
        let mut e = Engine::new(cfg());
        let end = 1_800_000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(end)],
            now_ms: 1_000,
        });
        // Simulate an in-flight entry: the rollover path clears this set
        // (re-arming duplicate entries mid-round); a top-up must not.
        e.pending_tokens.insert((
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "up".into(),
        ));
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(end), eth_market(end)],
            now_ms: 61_000,
        });
        assert!(
            e.pending_tokens.contains(&(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                "up".into()
            )),
            "the top-up must not re-arm an entry that is already pending"
        );

        // The next real rollover still clears it.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(2_700_000)],
            now_ms: 1_800_000,
        });
        assert!(
            !e.pending_tokens.contains(&(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                "up".into()
            )),
            "a genuine rollover keeps its historic semantics"
        );
        assert_eq!(e.scanner.markets().len(), 1, "a new slot replaces the list");
    }

    fn venue_market(end_ms: i64, venue: &str) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: format!("cond-{venue}"),
            question_id: format!("q-{venue}"),
            up_token_id: format!("{venue}-up"),
            down_token_id: format!("{venue}-down"),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: venue.into(),
            neg_risk: true,
            question: "BTC up or down".into(),
        }
    }

    #[test]
    fn two_venues_same_slot_merge_and_rollover_per_venue() {
        // Multi-plugin data plane: polymarket and kalshi both list slot 2
        // (staggered). Each feed is a top-up for ITS venue only; a different
        // slot from ONE venue rolls over only that venue's markets.
        let rounds = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(RoundProbe {
                rounds: rounds.clone(),
            }),
            "test".into(),
        )
        .unwrap();
        let end = 1_800_000; // slot 2 (1_800_000 ms / 900 s grid)
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(end, "polymarket")],
            now_ms: 1_000,
        });
        assert_eq!(*rounds.lock().unwrap(), vec![2]);

        // Kalshi lists the same slot a minute later: it must JOIN the scanner
        // (top-up), and the polymarket markets must survive the merge.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(end, "kalshi")],
            now_ms: 61_000,
        });
        assert_eq!(
            e.scanner.markets().len(),
            2,
            "the second venue's same-slot feed is a merge, not a rollover"
        );
        assert!(
            e.scanner.markets().iter().any(|m| m.venue == "polymarket"),
            "the first venue's markets are not wiped by the sibling feed"
        );
        assert_eq!(
            *rounds.lock().unwrap(),
            vec![2],
            "a same-slot cross-venue feed is not a round transition"
        );
        // The merge path must ALSO observe the second venue's own expiry —
        // the merge comment's "the first feed already observed the round's
        // one expiry" is a single-venue assumption. In a mixed-venue table
        // the staggered top-up is usually the ONLY declaration the second
        // venue ever makes (its first feed hits the merge, never the
        // rollover below), so skipping the observation leaves it out of
        // `observed_venue_keys()` forever: no panel `venueRounds` row (the
        // countdown rebind waits for it and shows 00:00), and
        // `round_timing_for_venue` falls back to the sibling's clock.
        assert!(
            e.scanner()
                .observed_venue_keys()
                .contains(&"kalshi".to_string()),
            "the second venue's same-slot top-up must observe ITS OWN expiry \
             (its first feed never reaches the rollover path)"
        );

        // Kalshi lists the NEXT slot while polymarket still sits in slot 2:
        // kalshi's rollover fires on_round and replaces ITS OWN markets, while
        // the polymarket markets stay live in the table.
        let next_end = 2_700_000; // slot 3
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(next_end, "kalshi")],
            now_ms: 1_800_000,
        });
        assert_eq!(
            *rounds.lock().unwrap(),
            vec![2, 3],
            "the single-venue slot change is a real rollover"
        );
        let venues: Vec<&str> = e
            .scanner
            .markets()
            .iter()
            .map(|m| m.venue.as_str())
            .collect();
        assert_eq!(
            venues,
            vec!["polymarket", "kalshi"],
            "the rollover replaces the feeding venue's list; the sibling survives"
        );
        // ...and the polymarket clock/expiry observation survives in its own
        // slot, reachable for the sibling's timing decisions.
        assert!(
            e.scanner()
                .round_timing_for_venue("polymarket", 1_000_001)
                .expires_at_ms
                > 0,
            "the sibling venue's clock slot survives the kalshi rollover"
        );
    }

    /// 2026-10-25 poly 轮钟冻结定谳（生产档案取证）：PolySimulator 镜像
    /// Polymarket —— 同窗口 condition/token id 逐字节相同。PM 批先落表，
    /// poly 批同 slot 到达时 merge 臂的 fresh 集为空 → 旧代码静默 return，
    /// (polysimulator, 900) 轮钟永不落地（venueRounds 缺行、面板倒计时
    /// 永远绑不上、切账户永久「加载中」）、poly 行也永远进不了表。
    /// 修复语义：fresh 空的 merge 批次仍要 (a) 给自己的 (venue, duration)
    /// 钟开口（observed 幂等检查挡真正的同所重复申报），(b) 把整批存进
    /// 镜像显示簿，行情装配层据此合成带它标签的行情行。表保持 token 唯一
    /// —— 镜像即同一市场，交易面一份行就够。
    fn mirror_market(end_ms: i64, venue: &str) -> CryptoMarket {
        // Same tokens for both venues — the mirror property itself.
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond-mirror".into(),
            question_id: "q-mirror".into(),
            up_token_id: "mirror-up".into(),
            down_token_id: "mirror-down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: venue.into(),
            neg_risk: false,
            question: "BTC up or down".into(),
        }
    }

    #[test]
    fn a_mirror_venues_same_slot_batch_still_opens_its_clock_and_display_rows() {
        let mut e = Engine::new(cfg());
        let end = 1_800_000; // slot 2
        // Polymarket lands first: a rollover puts its row in the table and
        // speaks its clock.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polymarket")],
            now_ms: 1_000,
        });
        assert_eq!(e.scanner.markets().len(), 1);

        // The mirror venue declares the SAME round — byte-identical tokens,
        // a different venue tag. Its batch is a same-slot merge whose fresh
        // set is EMPTY (every token already known).
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polysimulator")],
            now_ms: 61_000,
        });
        // The table stays token-unique — no duplicate mirror row.
        assert_eq!(
            e.scanner.markets().len(),
            1,
            "mirror tokens are one market; the table must not grow a second row"
        );
        // ...but the mirror venue's clock MUST have spoken: the panel's
        // venueRounds row and the countdown rebind wait for it.
        assert!(
            e.scanner()
                .observed_clock_keys()
                .iter()
                .any(|(v, d)| v == "polysimulator" && *d == 900),
            "a mirror venue's same-slot batch must open its own (venue, duration) clock \
             even when every token is already known"
        );
        // ...and its batch lands in the display book for the assembly to
        // synthesize its tagged rows.
        let shadow = e.shadow_markets_for("polysimulator", 900).expect(
            "the mirror batch must reach the display book (the table cannot carry its rows)",
        );
        assert_eq!(shadow.len(), 1);
        assert_eq!(shadow[0].venue, "polysimulator");

        // A genuine duplicate re-feed of the SAME venue does not disturb
        // anything: the clock is already open, no re-observe, no shadow churn.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polysimulator")],
            now_ms: 91_000,
        });
        assert!(e.shadow_markets_for("polysimulator", 900).is_some());
        assert_eq!(e.scanner.markets().len(), 1);

        // The next window: PM rolls over (real rows move), the mirror venue's
        // new-window batch again merges fresh-empty — its clock re-speaks for
        // the new expiry and the shadow rows refresh.
        let next_end = 2_700_000; // slot 3
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(next_end, "polymarket")],
            now_ms: 1_800_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(next_end, "polysimulator")],
            now_ms: 1_800_001,
        });
        assert_eq!(
            e.scanner.markets()[0].round_slot,
            next_end / 1000 / 900,
            "the table tracks the live window"
        );
        assert_eq!(
            e.scanner
                .round_timing_for("polysimulator", 900, 1_800_001)
                .expires_at_ms,
            next_end,
            "the mirror venue's clock follows its own new-window declaration"
        );
        assert_eq!(
            e.shadow_markets_for("polysimulator", 900).unwrap()[0].expires_at_ms,
            next_end,
            "the display book carries the live window's rows, not the stale ones"
        );
    }

    #[test]
    fn a_mirror_venue_shadow_rows_never_leak_into_trading_lookups() {
        // The display book is display-only: token→market lookups (the venue
        // admission chain reads these) must see exactly the table's rows —
        // the mirror table row is the first venue's.
        let mut e = Engine::new(cfg());
        let end = 1_800_000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polymarket")],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polysimulator")],
            now_ms: 61_000,
        });
        let m = e
            .scanner
            .market_for_token("mirror-up")
            .expect("the mirror token resolves through the table");
        assert_eq!(
            m.venue, "polymarket",
            "token lookups stay single-sourced on the table's row (first venue wins), \
             never on the display book"
        );
    }

    #[test]
    fn a_venue_that_missed_a_full_round_loses_its_stale_rows_at_the_next_rollover() {
        // #550 复现缺陷（症状①）的根因之一：换轮臂只替换 FEEDING venue 的
        // 行，兄弟行「存活到自己的 feed 来换」。存活是 staggered 上架的
        // 桥接语义（兄弟在轮界刚过、自己还没重报的几秒里不能丢行），
        // 但没有边界 —— 兄弟停喂（插件死/凭证失效）后，它的行带着已过期
        // 的 expiry 永久留在表里：行情页渲染已结束轮次的行，它的
        // observed expiry 还通过全局轮钟的 earliest_observed 把
        // time_left 钉死 0（全体新开仓被 Timing 门拦死）。
        //
        // 裁决语义：错满一整轮（slot 距离 ≥ 2）的兄弟行在下一次换轮时
        // 清出表；只差一格（刚过轮界还没重报）的兄弟行照旧桥接 ——
        // `two_venues_same_slot_merge_and_rollover_per_venue` 钉死的
        // 「sibling survives」在它的时序（slot 距离 1）下原样成立。
        let mut e = Engine::new(cfg());
        let end = 1_800_000; // slot 2
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(end, "polymarket")],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(end, "kalshi")],
            now_ms: 61_000,
        });
        assert_eq!(e.scanner.markets().len(), 2);

        // kalshi rolls to slot 3 at the exact slot boundary: polymarket has
        // NOT re-fed yet — one slot behind, the bridge window. Its row must
        // SURVIVE (the pinned staggered semantics).
        let next_end = 2_700_000; // slot 3
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(next_end, "kalshi")],
            now_ms: 1_800_000,
        });
        assert!(
            e.scanner.markets().iter().any(|m| m.venue == "polymarket"),
            "a sibling one slot behind is inside the staggered bridge and survives"
        );

        // kalshi rolls again and polymarket still has not spoken (dead feed):
        // two slots behind = it missed a full round. Its finished-round row
        // must now leave the table.
        let end4 = 3_600_000; // slot 4
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(end4, "kalshi")],
            now_ms: 2_700_000,
        });
        let venues: Vec<&str> = e
            .scanner
            .markets()
            .iter()
            .map(|m| m.venue.as_str())
            .collect();
        assert_eq!(
            venues,
            vec!["kalshi"],
            "a venue that missed a full round must not keep its finished-round row: {venues:?}"
        );
    }

    // ── B1c: multi-family rounds — the engine plane ────────────────────────

    /// A market row carrying an EXPLICIT duration family: same shape as
    /// [`venue_market`], but the row stamps its own `round_duration_sec` and
    /// its slot is computed on ITS OWN grid (a 1h row's slot numbers and a
    /// 15m row's slot numbers are drawn from different rulers).
    fn family_market(end_ms: i64, venue: &str, duration_sec: i64) -> CryptoMarket {
        let asset = if duration_sec == 900 { "BTC" } else { "ETH" };
        CryptoMarket {
            asset: asset.into(),
            condition_id: format!("cond-{venue}-{duration_sec}"),
            question_id: format!("q-{venue}-{duration_sec}"),
            up_token_id: format!("{venue}-{duration_sec}-up"),
            down_token_id: format!("{venue}-{duration_sec}-down"),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / duration_sec,
            round_duration_sec: duration_sec,
            archive_verdict: false,
            venue: venue.into(),
            neg_risk: true,
            question: "up or down".into(),
        }
    }

    #[test]
    fn a_same_slot_number_from_another_family_is_a_rollover_not_a_merge() {
        // B1c merge discipline: slot equality alone is venue-blind AND
        // family-blind — the 900s grid's slot N and the 3600s grid's slot N
        // are two different wall-clock rounds (thousands of seconds apart).
        // A 1h feed whose slot NUMBER collides with the resident 15m slot
        // must take the rollover arm (replacing ITS family's rows), never
        // the merge arm (which would graft a 1h market into a live 15m
        // round and skip its on_round).
        let rounds = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(RoundProbe {
                rounds: rounds.clone(),
            }),
            "test".into(),
        )
        .unwrap();
        // The 15m family feeds first: slot 2 (1_800_000 ms / 900 s).
        let end15 = 1_800_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end15, "polymarket", 900)],
            now_ms: 1_000,
        });
        assert_eq!(*rounds.lock().unwrap(), vec![2]);

        // The 1h family feeds a market whose SLOT NUMBER is also 2 — the
        // 3600s grid's slot 2 covers [7200s, 10800s), hours away from the
        // 15m slot 2 ([1800s, 2700s)). It must NOT merge: it is a real
        // rollover of its own family.
        let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end1h, "polymarket", 3600)],
            now_ms: 1_000,
        });
        assert_eq!(
            *rounds.lock().unwrap(),
            vec![2, 2],
            "the colliding slot NUMBER from another family is a real rollover \
             of that family — on_round refires"
        );
        assert_eq!(
            e.scanner.markets().len(),
            2,
            "the 1h row joins the table (its family is new); the 15m row stays"
        );
        assert!(
            e.scanner
                .markets()
                .iter()
                .any(|m| m.round_duration_sec == 900),
            "the 15m row survives the 1h family's rollover"
        );
    }

    #[test]
    fn a_same_slot_same_family_feed_still_merges() {
        // The green half of the merge pair: within ONE family the slot
        // number is the round identity — a staggered top-up from the same
        // (venue, duration) family still merges (the #498 semantics the
        // duration check must not break).
        let rounds = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(RoundProbe {
                rounds: rounds.clone(),
            }),
            "test".into(),
        )
        .unwrap();
        let end15 = 1_800_000i64; // 900s-grid slot 2
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end15, "polymarket", 900)],
            now_ms: 1_000,
        });
        assert_eq!(*rounds.lock().unwrap(), vec![2]);
        // The SAME family's second asset, listed a minute into the round —
        // its own tokens (the top-up filter grafts by unknown token ids).
        let eth15 = CryptoMarket {
            asset: "ETH".into(),
            condition_id: "cond-poly-900-eth".into(),
            question_id: "q-poly-900-eth".into(),
            up_token_id: "polymarket-900-eth-up".into(),
            down_token_id: "polymarket-900-eth-down".into(),
            question: "ETH up or down".into(),
            ..family_market(end15, "polymarket", 900)
        };
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![eth15],
            now_ms: 61_000,
        });
        assert_eq!(
            e.scanner.markets().len(),
            2,
            "a same-family same-slot top-up merges"
        );
        assert_eq!(
            *rounds.lock().unwrap(),
            vec![2],
            "a same-family top-up is not a round transition"
        );
    }

    #[test]
    fn family_a_rolling_over_does_not_reset_family_b_feeding_state() {
        // B1c rollover discipline: the feeding-state reset (pending tokens,
        // staged candidates) used to be global — ANY venue's rollover wiped
        // EVERY family's pending entries and re-armed duplicates mid-round.
        // The reset is now scoped to the feeding family: a 1h rollover must
        // leave a 15m token's pending latch and staged candidate untouched.
        let mut e = Engine::new(cfg());
        let end15 = 1_800_000i64; // 900s-grid slot 2
        let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![
                family_market(end15, "polymarket", 900),
                family_market(end1h, "polymarket", 3600),
            ],
            now_ms: 1_000,
        });
        // An in-flight 15m entry (the state the old whole-clear wiped) plus
        // a staged 15m candidate (Issue #390's replay staging).
        e.pending_tokens.insert((
            blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            "polymarket-900-up".into(),
        ));
        e.last_candidates = vec![test_signal("polymarket-900-up")];
        e.candidates_stable = true;

        // The 1h family rolls over its next slot: the feeding family is
        // (polymarket, 3600) — the 15m family's feeding state must survive.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end1h + 3_600_000, "polymarket", 3600)],
            now_ms: end1h,
        });
        assert!(
            e.pending_tokens.contains(&(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                "polymarket-900-up".into()
            )),
            "family A's rollover must not re-arm family B's pending entry"
        );
        assert!(
            e.last_candidates
                .iter()
                .any(|sig| sig.token_id == "polymarket-900-up"),
            "family A's rollover must not retire family B's staged candidate"
        );
        assert!(
            e.candidates_stable,
            "the surviving staged set keeps its stability flag"
        );

        // The 15m family's OWN rollover still clears it — the historic
        // semantics, scoped.
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end15 + 900_000, "polymarket", 900)],
            now_ms: end15,
        });
        assert!(
            !e.pending_tokens.contains(&(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                "polymarket-900-up".into()
            )),
            "the family's own rollover keeps its reset semantics"
        );
        assert!(
            !e.last_candidates
                .iter()
                .any(|sig| sig.token_id == "polymarket-900-up"),
            "the family's own rollover retires its staged candidates"
        );
        assert!(
            !e.candidates_stable,
            "an emptied staged set is not stable — the historic whole-clear verdict"
        );
    }

    #[test]
    fn a_1h_rollover_does_not_reset_the_clock_of_the_15m_family() {
        // The clock half of the family-scoped rollover: the feeding batch's
        // expiry declaration lands in ITS (venue, duration) family slot and
        // the time_left handed to on_round comes from that family's clock —
        // a 1h rollover must never speak into the 15m family's clock (the
        // B1a keying the engine reset now aligns with).
        let mut e = Engine::new(cfg());
        let end15 = 1_800_000i64;
        let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end15, "polymarket", 900)],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end1h, "polymarket", 3600)],
            now_ms: 1_000,
        });
        // Both families spoke; the 15m clock still reads ITS family's expiry.
        assert!(
            e.scanner()
                .observed_clock_keys()
                .contains(&("polymarket".to_string(), 900)),
            "the 15m family's declaration stays keyed (polymarket, 900)"
        );
        assert!(
            e.scanner()
                .observed_clock_keys()
                .contains(&("polymarket".to_string(), 3600)),
            "the 1h family's declaration lands in its own key"
        );
        assert_eq!(
            e.scanner()
                .round_timing_for("polymarket", 900, 1_000)
                .expires_at_ms,
            end15,
            "the 1h rollover never moves the 15m family's clock"
        );
    }

    #[test]
    fn stale_rows_are_measured_on_their_own_duration_grid() {
        // #550 on multi-family tables: the "missed a full round" distance
        // used ONE global ruler (slot-number subtraction), so a 1h sibling's
        // slot N sat arbitrarily far from a 15m feed's slot N and its row
        // was judged by numbers from two different grids. Each row is now
        // measured against the feeding round's start CONVERTED onto the
        // row's own grid.
        let mut e = Engine::new(cfg());
        // A 1h row resident at 3600s-grid slot 2 (covers [7200s, 10800s)).
        let end1h = 2 * 3600 * 1000; // 3600s-grid slot 2 covers [7200s, 10800s)
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(end1h, "polymarket", 3600)],
            now_ms: 1_000,
        });
        // A 15m feed deep into the 1h round — 900s-grid slot 10 starts at
        // 9000s, still INSIDE the 1h slot-2 round (which runs to 10800s).
        // On each row's own grid: the 1h row's lag = (10 × 900 / 3600) − 2
        // = 2 − 2 = 0 — well inside the bridge, the row MUST survive.
        let feed15 = 10 * 900 * 1000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(feed15, "kalshi", 900)],
            now_ms: 9_000_000,
        });
        assert!(
            e.scanner
                .markets()
                .iter()
                .any(|m| m.round_duration_sec == 3600),
            "the 1h row is mid-round on ITS OWN grid and must not be judged \
             stale by 15m slot arithmetic"
        );
        assert!(
            e.scanner
                .markets()
                .iter()
                .any(|m| m.round_duration_sec == 900),
            "the feeding 15m row is in the table"
        );
    }

    #[test]
    fn legacy_zero_duration_rows_keep_the_global_grid_verdict() {
        // Byte-identical duty: rows and feeds that declare NO duration (the
        // pre-B1a corpus, `round_duration_sec == 0`) resolve to the global
        // grid, so the family-scoped stale-row arithmetic degenerates to the
        // historic `slot - round_slot < 2` for every row — the existing
        // corpus replays unchanged.
        let mut e = Engine::new(cfg());
        let mut row = venue_market(1_800_000, "kalshi");
        row.round_duration_sec = 0; // the legacy shape
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![row],
            now_ms: 1_000,
        });
        let mut late = venue_market(1_800_000, "polymarket");
        late.round_duration_sec = 0;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![late], // same slot: merges (the #498 bridge)
            now_ms: 61_000,
        });
        assert_eq!(
            e.scanner.markets().len(),
            2,
            "zero-duration rows merge on the global grid, exactly as before"
        );
        // Two slots behind → dropped, the #550 boundary unchanged: the ghost
        // row sits in slot 2 and the next feed (polymarket, a DIFFERENT
        // venue) rolls to slot 4 — the ghost missed a full round and its
        // zero-duration rows leave the table on the global grid, exactly as
        // the pre-B1c #550 verdict did.
        let mut dead = venue_market(1_800_000, "ghost");
        dead.round_duration_sec = 0;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![dead],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![venue_market(3_600_000, "polymarket")], // slot 4
            now_ms: 2_700_000,
        });
        assert!(
            !e.scanner.markets().iter().any(|m| m.venue == "ghost"),
            "the global-grid verdict for zero-duration rows is untouched"
        );
        assert!(
            e.scanner.markets().iter().any(|m| m.venue == "polymarket"),
            "the feeding venue's own rows survive, as before"
        );
    }

    #[test]
    fn one_venue_expiring_does_not_freeze_the_other_venue() {
        // Both venues observed their (identical-grid) expiries. At a clock
        // inside polymarket's force-exit window, a polymarket candidate is
        // refused while a kalshi candidate (same grid, later observed expiry)
        // is not — the per-market timing gate reads the market's OWN venue
        // clock instead of one table-wide verdict.
        let mut s = Scanner::new(ScannerConfig {
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 180,
            ..Default::default()
        });
        let poly_end = 1_800_000i64;
        let kalshi_end = 1_950_000i64; // a later observed close, in-band
        s.observe_end_time_for_venue("polymarket", poly_end, 900_000);
        s.observe_end_time_for_venue("kalshi", kalshi_end, 900_000);
        let poly = venue_market(poly_end, "polymarket");
        let kalshi = venue_market(kalshi_end, "kalshi");
        s.set_markets(vec![poly.clone(), kalshi.clone()]);

        // t = 1_630_000: 170s to the POLYMARKET close (< 180 → blocked), 320s
        // to the KALSHI close (≥ 180 → ok).
        let now = 1_630_000i64;
        assert_eq!(
            s.can_trade_reason_for(&poly, now),
            Err(TimingBlock::TooCloseToExpiry {
                time_left_sec: 170,
                min_time_left_sec: 180
            }),
            "the polymarket market is inside its own force-exit window"
        );
        assert!(
            s.can_trade_reason_for(&kalshi, now).is_ok(),
            "the kalshi market must NOT be frozen by the polymarket window"
        );
        // The whole-table verdict stays conservative (earliest expiry).
        assert!(
            s.can_trade_reason(now).is_err(),
            "the global window keeps the earliest-expiry semantics"
        );
    }

    #[test]
    fn a_venue_bound_scope_admits_untagged_markets() {
        // The exit-economics regression: a factory account book binds default
        // to a venue, the corpus predates #427's venue tagging (serde default
        // = empty), and `scope_admits_token` refused EVERY token — signals
        // 89975 → 0, closes 13 → 0 across all four windows. An untagged
        // market says nothing the binding could disagree with (the single-
        // plugin world's venue was implicit), so it admits every scope; the
        // WRONG tagged venue still refuses, which is the gate's actual job.
        let mut e = Engine::new(cfg());
        let end = 1_800_000i64;
        e.scanner.set_markets(vec![
            venue_market(end, "polymarket"),
            venue_market(end, "kalshi"),
            eth_market(end), // untagged (serde default), like old corpora
        ]);
        e.set_account_venue("default", "polymarket");

        assert!(
            e.scope_admits_token("default", "polymarket-up"),
            "the bound venue's own tagged market admits"
        );
        assert!(
            e.scope_admits_token("default", "eth-up"),
            "an UNTAGGED market admits a bound scope — the pre-tag corpus case"
        );
        assert!(
            !e.scope_admits_token("default", "kalshi-up"),
            "another venue's tagged market still refuses — the gate keeps its teeth"
        );
        assert!(
            !e.scope_admits_token("default", "no-such-token"),
            "an unknown token (no market) refuses, as before"
        );
        assert!(
            e.scope_admits_token("unbound", "kalshi-up"),
            "an unbound scope admits everything, as before"
        );
    }

    #[test]
    fn a_mirror_bound_scope_admits_through_the_mirror_declaration() {
        // The polysimulator account's zero-signal root cause: the table is
        // token-unique and the PM batch lands first, so the mirror token's
        // row is tagged `polymarket` forever — a polysimulator-bound scope
        // looked the row up, saw "another venue", and silently dropped every
        // candidate (signals=0, blocked=0). The plugin's OWN `mirror_of`
        // declaration is the evidence it really trades the token: it admits.
        // The display book (shadow_markets) is NOT consulted — #643 retired
        // the shadow-batch fallback, admission reads the declaration table.
        let mut e = Engine::new(cfg());
        let end = 1_800_000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polymarket")],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polysimulator")],
            now_ms: 61_000,
        });
        e.set_venue_mirror("polysimulator", "polymarket");
        e.set_account_venue("poly", "polysimulator");
        e.set_account_venue("pm", "polymarket");

        assert!(
            e.scope_admits_token("poly", "mirror-up"),
            "the mirror-bound scope admits through the venue's mirror_of declaration"
        );
        assert!(
            e.scope_admits_token("pm", "mirror-up"),
            "the mirrored venue's own row admits its binding, as before"
        );
        assert!(
            !e.scope_admits_token("poly", "polymarket-up"),
            "a NON-mirror token of another venue still refuses (no declaration names it)"
        );
        assert!(
            e.execution_venue_for("poly", "mirror-up") == Some("polysimulator".into()),
            "execution follows the placing account's binding, not the table row"
        );
        assert!(
            e.execution_venue_for("pm", "mirror-up") == Some("polymarket".into()),
            "the mirrored venue's binding reads its own row, unchanged"
        );
        assert_eq!(
            e.execution_venue_for("unbound", "mirror-up"),
            Some("polymarket".into()),
            "an unbound account rides the plain table lookup"
        );
    }

    #[test]
    fn a_shadow_batch_without_a_mirror_declaration_never_admits_or_reroutes() {
        // #643's contract, pinned in reverse: WITHOUT a mirror_of declaration
        // the shadow batch (the display book — the mirror venue's feed was
        // merged away as all-known tokens) must NOT open the scope gate nor
        // swing the execution routing, however fresh the observation is.
        // Runtime observation wobbles with the feed (plugin dead, credentials
        // expired → the batch drops out); the declaration does not. The
        // display book's rows stay display-only here, exactly as they are in
        // the books_view arm.
        let mut e = Engine::new(cfg());
        let end = 1_800_000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polymarket")],
            now_ms: 1_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![mirror_market(end, "polysimulator")],
            now_ms: 61_000,
        });
        e.set_account_venue("poly", "polysimulator");
        // Pre-condition: the display book actually carries the shadow batch —
        // the refusal below is judged against live display evidence, not an
        // empty book.
        assert!(
            e.shadow_markets_for("polysimulator", 900).is_some(),
            "the fixture must have the shadow batch in the display book"
        );
        assert!(
            !e.scope_admits_token("poly", "mirror-up"),
            "the shadow batch alone must not admit the mirror-bound scope"
        );
        assert_eq!(
            e.execution_venue_for("poly", "mirror-up"),
            Some("polymarket".into()),
            "routing stays on the table row — no shadow-batch override"
        );
        // Routing is not perturbed for the other bindings either: the display
        // book changes nothing about who executes the token.
        e.set_account_venue("pm", "polymarket");
        assert!(
            e.execution_venue_for("pm", "mirror-up") == Some("polymarket".into()),
            "the mirrored venue's binding keeps its plain table routing"
        );

        // The declaration is the ONLY key that opens the gate: with it (the
        // plugin's own `mirror_of` fact, pushed at assembly) the evidence is
        // CONSTRUCTIVE — it survives the shadow batch going away entirely
        // (the parasitic shape, where the mirror plugin declares no rounds at
        // all and the shadow batch can never exist).
        e.set_venue_mirror("polysimulator", "polymarket");
        assert!(
            e.scope_admits_token("poly", "mirror-up"),
            "the static mirror declaration admits the mirrored row without any shadow batch"
        );
        assert!(
            e.execution_venue_for("poly", "mirror-up") == Some("polysimulator".into()),
            "execution still follows the binding under the static mirror declaration"
        );
        assert!(
            !e.scope_admits_token("poly", "polymarket-up"),
            "a NON-mirror token of another venue still refuses under the static declaration"
        );
        e.shadow_markets.clear();
        assert!(
            e.scope_admits_token("poly", "mirror-up"),
            "the declaration admits with the display book gone — constructive, not observed"
        );

        // The declaration is the plugin's identity: clearing it (or a
        // self-mirror/empty spelling) removes the evidence again.
        e.set_venue_mirror("polysimulator", "");
        assert!(
            !e.scope_admits_token("poly", "mirror-up"),
            "an empty mirror spelling clears the declaration"
        );
        assert_eq!(
            e.execution_venue_for("poly", "mirror-up"),
            Some("polymarket".into()),
            "routing falls back to the table row once the declaration is gone"
        );
    }

    // ── E3 (轮次多族收尾): strategy-visible round clocks follow the market's
    //    own declared family ────────────────────────────────────────────────

    #[test]
    fn each_family_reads_its_own_clock_in_the_gate_trail() {
        // The audit's E3 finding: the strategy-visible round clock (the D-31
        // waiver line, the near-miss/block telemetry, the emitted order's
        // slot) read the GLOBAL grid, so a 1h market's candidate was judged
        // by 15m numbers. Two families, one venue, one evaluation: the 15m
        // candidate is inside ITS force-exit window (172s left on the 900s
        // grid) while the 1h candidate has 3772s left on ITS grid — the same
        // `timing: true` declaration must block one and waive the other,
        // each on its OWN family clock.
        //
        // Fixture arithmetic (cfg grid 900s, min_time_left 180s):
        //   feed 1 @ 7_000_000ms: pm/900 row, expiry 7_200_000 (900s slot 7's
        //   grid end; offset[pm] ← 0). Feed 2 @ 7_028_000ms: pm/3600 row,
        //   expiry 10_800_000 (offset[pm] ← 3_600_000, the 1h schedule).
        //   Legacy-bucket clocks (offsetless "") at 7_028_000:
        //     900s family → slot 7, effective earliest same-duration
        //     declaration 7_200_000 → time_left 172 < 180 (BLOCK);
        //     3600s family → slot 1, effective 10_800_000 → time_left 3_772
        //     ≥ 180 (WAIVE). The pre-E3 code read the global clock for both
        //     waivers: 172 < 180 → BOTH blocked, zero orders.
        let mut cfg = cfg();
        cfg.scanner.min_time_left_sec = 180;
        let mut e = Engine::new(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "btc_fade",
                dec!(0.45),
                &["BTC"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "eth_fade",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("btc_fade", true));
        assert!(e.set_strategy_enabled("eth_fade", true));

        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(7_200_000, "polymarket", 900)],
            now_ms: 7_000_000,
        });
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![family_market(10_800_000, "polymarket", 3600)],
            now_ms: 7_028_000,
        });
        dip_only(&mut e, "polymarket-900-up", 7_028_000);
        dip_only(&mut e, "polymarket-3600-up", 7_028_000);

        let orders = e.evaluate(7_028_000);
        // The 1h candidate enters (its family says 3772s left); the 15m one
        // is blocked (its family says 172s left).
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "eth_fade");
        assert_eq!(
            orders[0].round_slot, 1,
            "the emitted order stamps the 3600s FAMILY's slot, not the global grid's"
        );
        assert_eq!(
            orders[0].internal_key, "eth_fade:ETH:up:1",
            "the internal key's round segment is the candidate family's slot"
        );
        let blocked = e.last_blocked();
        assert_eq!(blocked.len(), 1, "{blocked:?}");
        assert_eq!(blocked[0].strategy, "btc_fade");
        assert_eq!(blocked[0].reason, BlockReason::Timing);
        assert_eq!(
            blocked[0].time_left_sec, 172,
            "the near-miss/block telemetry reads the 900s family's time_left"
        );
        // The audit trail: the honoured waiver is the 1h candidate's, and its
        // `time_left_sec` is the FAMILY's number (3772s), not the global 172
        // that used to deny the waiver.
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert_eq!(ex[0].strategy, "eth_fade");
        assert_eq!(ex[0].gate, "timing");
        assert_eq!(ex[0].time_left_sec, 3_772);
        // E25 projection: the cycle's table-family clock (first row = the
        // 900s family on the legacy bucket).
        assert_eq!(e.last_time_left_sec(), 172);
    }

    #[test]
    fn single_family_tables_read_the_global_numbers_verbatim() {
        // Byte-identical duty (the replay-baseline red line): every corpus is
        // single-family, where `row_family_duration` resolves EVERY row to the
        // configured grid and the legacy-bucket read degenerates to the
        // global `round_timing()` — so the waiver record, the emitted
        // order's slot/key and the E25 projection must equal the scanner's
        // global clock exactly. A zero-duration row (the pre-B1a corpus
        // shape) resolves the same way.
        for declared in [900i64, 0] {
            let mut cfg = cfg();
            cfg.scanner.min_round_age_sec = 10_000; // force the TooYoung block
            cfg.scanner.min_time_left_sec = 180;
            let mut e = Engine::new(cfg);
            e.register_user_strategy(
                Box::new(DipBuyer::exempting(
                    "fade",
                    dec!(0.45),
                    &["BTC"],
                    GateExemptions {
                        timing: true,
                        momentum: false,
                        timing_min_time_left_sec: None,
                    },
                )),
                "test".into(),
            )
            .unwrap();
            assert!(e.set_strategy_enabled("fade", true));
            let mut row = market(1_800_000);
            row.round_duration_sec = declared;
            e.on_data(DataEvent::RoundMarkets {
                markets: vec![row],
                now_ms: 1_400_000,
            });
            dip_only(&mut e, "up", 1_500_000);

            let orders = e.evaluate(1_500_000);
            // The scanner's global clock — the exact numbers the pre-E3 code
            // fed every one of these sites.
            let global = e.scanner().round_timing(1_500_000);
            assert_eq!(orders.len(), 1, "declared {declared}: {orders:?}");
            assert_eq!(orders[0].round_slot, global.slot);
            assert_eq!(
                orders[0].internal_key,
                format!("fade:BTC:up:{}", global.slot)
            );
            let ex = e.last_exemptions();
            assert_eq!(ex.len(), 1, "declared {declared}: {ex:?}");
            assert_eq!(ex[0].gate, "timing");
            assert_eq!(
                ex[0].time_left_sec, global.time_left_sec,
                "declared {declared}: the waiver line reads the global clock's time_left"
            );
            assert_eq!(e.last_time_left_sec(), global.time_left_sec);
        }
    }

    fn cfg() -> EngineConfig {
        EngineConfig {
            scanner: ScannerConfig {
                assets: vec!["BTC".into()],
                round_duration_sec: 900,
                min_round_age_sec: 0,
                min_time_left_sec: 0,
            },
            trend: TrendConfig {
                confirm_sec: 5,
                ratio: dec!(0.5),
                min_price: dec!(0.5),
                broken_price: dec!(0.35),
                window_floor_ms: 0,
            },
            spread_arb: SpreadArbConfig {
                trend_max_entry_price: dec!(0.45),
                ..Default::default()
            },
            max_orderbook_stale_ms: DEFAULT_MAX_ORDERBOOK_STALE_MS,
            momentum_window_sec: 30,
            momentum_tol_pct: dec!(0.03),
            size_usd: dec!(2.5),
            min_shares: dec!(10),
            max_shares: dec!(10),
            size_pct: Decimal::ZERO,
            strategy_sizes: HashMap::new(),
        }
    }

    fn market(end_ms: i64) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond".into(),
            question_id: "q".into(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: "BTC up or down".into(),
        }
    }

    fn core() -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(5),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            ..Default::default()
        });
        // Keep the exit engine out of the way for signal tests.
        let cc = c.config_mut();
        cc.auto_exits_enabled = false;
        c
    }

    /// An engine with the three TEST-ONLY adapters hosted (`spread_arb` enabled,
    /// the other two registered-but-off) — the dispatch shape the deleted
    /// builtins had, so these tests keep exercising candidate dispatch, gates
    /// and hot parameters. The kernel itself ships no strategy (PR-B).
    fn engine() -> Engine {
        engine_from(cfg())
    }

    fn engine_from(cfg: EngineConfig) -> Engine {
        let mut e = Engine::new(cfg.clone());
        crate::strategies::test_support::host(&mut e, cfg.trend, cfg.spread_arb);
        e
    }

    /// The fast path's all_hold precondition counts only ENABLED strategies:
    /// a disabled non-holder contributes nothing during a replay (no
    /// evaluation, no orders, no positions — its hook output is dropped), so
    /// it must not veto the maintenance-gating skip. A registry of nine
    /// libraries with one enabled holder is exactly the deployment shape the
    /// fast path exists for.
    #[test]
    fn all_strategies_hold_to_settlement_ignores_disabled_strategies() {
        let mut e = Engine::new(cfg());
        crate::strategies::test_support::host_disabled(&mut e, cfg().trend, cfg().spread_arb);
        // Registered but every one disabled: no replay position can exist —
        // the enabled-holder set is empty, so the precondition holds vacuously.
        assert!(e.all_strategies_hold_to_settlement());
        // Enabling the (non-holder) dip buyer vetoes the fast path.
        assert!(e.set_strategy_enabled("spread_arb", true));
        assert!(!e.all_strategies_hold_to_settlement());
    }

    /// The host-pushed position view is sliced PER STRATEGY inside `evaluate`:
    /// each strategy's `StrategyCtx::held_positions` carries only rows whose
    /// `strategy` names it — a package can never read another package's book
    /// through the view (the `bk.holdings()` isolation contract).
    #[test]
    fn evaluate_slices_the_position_view_by_strategy_name() {
        struct HoldingsProbe {
            name: &'static str,
            seen: Arc<Mutex<Vec<String>>>,
        }
        impl EngineStrategy for HoldingsProbe {
            fn name(&self) -> &str {
                self.name
            }
            fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
            fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
            fn find_candidates(
                &mut self,
                ctx: &StrategyCtx<'_>,
            ) -> Vec<crate::signal::TradeSignal> {
                let tokens: Vec<String> = ctx
                    .held_positions()
                    .iter()
                    .map(|p| p.token_id.clone())
                    .collect();
                self.seen.lock().unwrap().push(tokens.join(","));
                Vec::new()
            }
        }

        let seen_a = Arc::new(Mutex::new(Vec::new()));
        let seen_b = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(HoldingsProbe {
                name: "probe_a",
                seen: Arc::clone(&seen_a),
            }),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(
            Box::new(HoldingsProbe {
                name: "probe_b",
                seen: Arc::clone(&seen_b),
            }),
            "test".into(),
        )
        .unwrap();
        e.set_strategy_enabled("probe_a", true);
        e.set_strategy_enabled("probe_b", true);
        e.set_positions_view(vec![
            HeldPosition {
                strategy: "probe_a".into(),
                account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                condition_id: "cond".into(),
                token_id: "tok_a".into(),
                direction: "up".into(),
                shares: "1.00".into(),
                entry_price: "0.42".into(),
                round_slot: 1,
            },
            HeldPosition {
                strategy: "probe_b".into(),
                account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                condition_id: "cond".into(),
                token_id: "tok_b".into(),
                direction: "down".into(),
                shares: "2.00".into(),
                entry_price: "0.44".into(),
                round_slot: 1,
            },
        ]);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: 1_000,
        });
        e.evaluate(1_000);
        assert_eq!(
            *seen_a.lock().unwrap(),
            vec!["tok_a".to_string()],
            "probe_a sees only its own position"
        );
        assert_eq!(
            *seen_b.lock().unwrap(),
            vec!["tok_b".to_string()],
            "probe_b sees only its own position"
        );
    }

    /// `release_pending_token` re-opens a token the kernel had suppressed:
    /// a partially-filled leg whose entry order ended may signal again THIS
    /// round (the gap-fill loop). Verified through the public surface: after
    /// `note_order_placed`, a standing candidate on the token does not count
    /// as standing; after the release, it does.
    #[test]
    fn release_pending_token_reopens_the_gate_after_a_terminal_order() {
        let mut e = Engine::new(cfg());
        e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_gap");
        e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_resting");
        // Both tokens suppressed: a standing set that references them is
        // entirely suppressed → `has_standing_candidates` is false.
        e.last_candidates = vec![test_signal("tok_gap"), test_signal("tok_resting")];
        assert!(
            !e.has_standing_candidates(),
            "every candidate's token has a live entry order"
        );
        // The terminal-order release: only the named token re-opens.
        e.release_pending_token("tok_gap");
        assert!(
            e.has_standing_candidates(),
            "the released token's candidate is standing again"
        );
    }

    /// The service-facing release is scoped to the STRATEGY that placed the
    /// order: a hold-to-settlement package gets its token re-opened (the
    /// gap-fill loop), a mid-round-exiting strategy does not (the historic
    /// one-entry-per-token-per-round discipline — the exit-economics windows
    /// degraded the moment spread_arb could re-enter after a terminal order).
    #[test]
    fn the_scoped_release_opens_holder_tokens_but_not_plain_strategies() {
        struct PlainHolder {
            name: String,
            holds: bool,
        }
        impl EngineStrategy for PlainHolder {
            fn name(&self) -> &str {
                &self.name
            }
            fn on_book(&mut self, _: &str, _: &crate::model::OrderbookSnapshot, _: i64) {}
            fn on_round(&mut self, _: i64, _: i64, _: i64) {}
            fn gate_exemptions(&self) -> GateExemptions {
                GateExemptions::none()
            }
            fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
                Vec::new()
            }
            fn holds_to_settlement(&self) -> bool {
                self.holds
            }
        }
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(PlainHolder {
                name: "plain".into(),
                holds: false,
            }),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(
            Box::new(PlainHolder {
                name: "pair_like".into(),
                holds: true,
            }),
            "test".into(),
        )
        .unwrap();
        e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_plain");
        e.note_scoped_order_placed(blitzkrieg_market_api::DEFAULT_ACCOUNT_ID, "tok_pair");
        assert!(
            !e.release_pending_token_for_strategy(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID,
                "plain",
                "tok_plain"
            ),
            "no release for a non-holder: the suppression stands"
        );
        assert!(
            e.pending_tokens.contains(&(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                "tok_plain".into()
            )),
            "the plain strategy's token stays latched"
        );
        assert!(
            e.release_pending_token_for_strategy(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID,
                "pair_like",
                "tok_pair"
            ),
            "a hold-to-settlement package's token is released"
        );
        assert!(
            !e.pending_tokens.contains(&(
                blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                "tok_pair".into()
            )),
            "the holder's token re-opens"
        );
    }

    fn test_signal(token: &str) -> TradeSignal {
        TradeSignal {
            strategy: "probe".into(),
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: token.into(),
            condition_id: "cond".into(),
            price: dec!(0.42),
            reason: "test".into(),
            shares: None,
            pair: None,
        }
    }

    #[test]
    fn engine_places_a_trend_confirmed_dip_buy() {
        let mut e = engine();
        let mut c = core();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });

        // Confirm the UP trend: several books with mid >= 0.5 within the window.
        for i in 0..12 {
            let t = now + i * 1000;
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: t,
            });
        }
        // Dip: mid 0.44 (bid 0.43) → entry 0.43 ≤ 0.45 → signal.
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        // Spot calm.
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: now + 12_000,
        });

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "expected one spread_arb entry");
        // Placing through the core succeeds (funds seeded) and fills on the book.
        let req = orders.into_iter().next().unwrap();
        let (id, _) = c.place(req, 5000, now + 12_000).unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn engine_skips_without_confirmed_trend() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Only one dip book; no confirm window yet.
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        });
        assert!(e.evaluate(now).is_empty());
    }

    #[test]
    fn engine_blocks_entry_when_spot_moves_against() {
        let mut e = engine();
        let _c = core();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        // Spot falls hard over the momentum window → UP entry blocked.
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: now + 1000,
        });
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(59000),
            now_ms: now + 12_000,
        });
        assert!(
            e.evaluate(now + 12_000).is_empty(),
            "spot moving against should block the entry"
        );
    }

    #[test]
    fn trend_break_is_reported_for_bid_cancellation() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        let broken = e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.30), dec!(100))],
            asks: vec![(dec!(0.32), dec!(100))],
            now_ms: now + 12_000,
        });
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "up");
    }

    #[test]
    fn records_near_miss_when_timing_gate_blocks_a_valid_dip() {
        // A valid trend-confirmed dip that the timing gate rejects must be
        // counted (this is the near-miss telemetry the shadow log lacks).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000; // force the timing gate to fail
        let mut e = engine_from(cfg);
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });

        let orders = e.evaluate(now + 12_000);
        assert!(orders.is_empty(), "timing gate must block the order");
        assert_eq!(
            e.last_blocked().len(),
            1,
            "the blocked dip should be recorded"
        );
        assert_eq!(e.last_blocked()[0].reason, BlockReason::Timing);
        e.tally_blocked();
        assert_eq!(e.blocked_timing_count(), 1);
        assert_eq!(e.blocked_momentum_count(), 0);
    }

    #[test]
    fn compute_shares_clamps_to_bounds() {
        let e = engine();
        // 2.5 / 0.25 = 10 shares within [10,10].
        assert_eq!(
            e.compute_shares(dec!(0.25), "spread_arb", "default"),
            dec!(10)
        );
        // Very low price would exceed max → clamped to 10.
        assert_eq!(
            e.compute_shares(dec!(0.01), "spread_arb", "default"),
            dec!(10)
        );
    }

    #[test]
    fn compute_shares_honours_custom_bounds() {
        // Small-balance live sizing: a 4-share fixed lot (2 positions on ~4.8u).
        let mut c = cfg();
        c.size_usd = dec!(2.5);
        c.min_shares = dec!(4);
        c.max_shares = dec!(4);
        let e = engine_from(c);
        // 2.5 / 0.45 ≈ 5.56 → rounds to 6, then clamped down to the 4-share lot.
        assert_eq!(
            e.compute_shares(dec!(0.45), "spread_arb", "default"),
            dec!(4)
        );
        // A wide band lets the nominal size win: 2.5 / 0.25 = 10 within [2,20].
        let mut c2 = cfg();
        c2.min_shares = dec!(2);
        c2.max_shares = dec!(20);
        let e2 = engine_from(c2);
        assert_eq!(
            e2.compute_shares(dec!(0.25), "spread_arb", "default"),
            dec!(10)
        );
        assert_eq!(
            e2.compute_shares(dec!(1.25), "spread_arb", "default"),
            dec!(2)
        );
    }

    // ── #202 equity-relative sizing ─────────────────────────────────────────

    /// A percentage engine. The absolute `size_usd` stays populated (2.5) even
    /// though the relative mode must ignore it, so any leak of the old budget
    /// into the ticket shows up as a wrong number here.
    fn pct_cfg(pct: Decimal, min_shares: Decimal, max_shares: Decimal) -> EngineConfig {
        let mut c = cfg();
        c.size_usd = dec!(2.5);
        c.size_pct = pct;
        c.min_shares = min_shares;
        c.max_shares = max_shares;
        c
    }

    #[test]
    fn equity_sizing_replaces_the_fixed_lot_and_follows_the_account() {
        let mut e = engine_from(pct_cfg(dec!(20), dec!(10), dec!(1000)));
        e.set_equity_usd(dec!(4.8));
        // 4.8 × 20% = 0.96 USD → 0.96/0.40 = 2.4 → 2 whole shares (floor: a
        // percentage budget is an upper bound). The absolute path sends 10
        // shares = 4.00 USD = 83% of this book.
        assert_eq!(
            e.compute_shares(dec!(0.40), "spread_arb", "default"),
            dec!(2)
        );
        assert_eq!(e.entry_budget_usd("spread_arb"), dec!(0.96));
        // The band's own floor (min_shares = 10) must NOT lift it back over the
        // budget — that floor is the #202 bug itself.
        assert_eq!(e.effective_sizing("spread_arb").min_shares, dec!(10));
        assert_eq!(
            e.compute_shares(dec!(0.40), "spread_arb", "default"),
            dec!(2)
        );
        // The same percentage on a 100× account is 100× the budget, and the lot
        // follows (the point of the knob: 4.8 → 480 must move the order size).
        e.set_equity_usd(dec!(480));
        assert_eq!(e.entry_budget_usd("spread_arb"), dec!(96));
        assert_eq!(
            e.compute_shares(dec!(0.40), "spread_arb", "default"),
            dec!(240)
        );
        // The old ceiling still wins when it is the tighter one: the shipped
        // [10, 10] band saturates even a 480 USD account at 10 shares (4.00 USD,
        // 0.8% of it), so arming `size_pct` means revisiting `max_shares`.
        let mut shipped = engine_from(pct_cfg(dec!(20), dec!(10), dec!(10)));
        shipped.set_equity_usd(dec!(480));
        assert_eq!(
            shipped.compute_shares(dec!(0.40), "spread_arb", "default"),
            dec!(10)
        );
    }

    #[test]
    fn equity_sizing_needs_a_budget_it_can_actually_spend() {
        // No equity pushed yet: sizing against an unknown account has no safe
        // reading, and falling back to the absolute lot would silently restore
        // the fixed 10 shares this knob exists to remove.
        let mut unknown = engine_from(pct_cfg(dec!(20), dec!(10), dec!(1000)));
        assert_eq!(
            unknown.compute_shares(dec!(0.40), "spread_arb", "default"),
            Decimal::ZERO
        );
        unknown.set_equity_usd(dec!(4.8));
        assert_eq!(
            unknown.compute_shares(dec!(0.40), "spread_arb", "default"),
            dec!(2)
        );
        // A budget that cannot buy one whole share (1% of 4.8 = 0.048 at 0.40)
        // emits NO order rather than a 0-size one that becomes a per-tick
        // rejection; the caller counts the skip.
        let mut tiny = engine_from(pct_cfg(dec!(1), dec!(10), dec!(1000)));
        tiny.set_equity_usd(dec!(4.8));
        assert_eq!(
            tiny.compute_shares(dec!(0.40), "spread_arb", "default"),
            Decimal::ZERO
        );
        assert_eq!(tiny.entry_ticket(dec!(0.40), "spread_arb", "default"), None);
        // A non-positive price and a zero ceiling are dead ends too, never a
        // panic or an unbounded ticket.
        assert_eq!(
            tiny.equity_shares(Decimal::ZERO, &tiny.global_sizing(), "default"),
            Decimal::ZERO
        );
        let mut closed = engine_from(pct_cfg(dec!(20), dec!(10), Decimal::ZERO));
        closed.set_equity_usd(dec!(4.8));
        assert_eq!(
            closed.compute_shares(dec!(0.40), "spread_arb", "default"),
            Decimal::ZERO
        );
    }

    #[test]
    fn per_strategy_equity_pct_is_clamped_by_the_global() {
        let over = |pct: Decimal, weight: Option<Decimal>| StrategySize {
            size_usd: None,
            min_shares: None,
            max_shares: None,
            size_weight: weight,
            size_pct: Some(pct),
        };
        // Global 20%: a 50% leg is clamped down to it, a 5% leg is kept.
        let mut c = pct_cfg(dec!(20), dec!(10), dec!(1000));
        c.strategy_sizes = [
            ("greedy".to_string(), over(dec!(50), None)),
            ("timid".to_string(), over(dec!(5), None)),
        ]
        .into();
        let mut e = engine_from(c);
        e.set_equity_usd(dec!(100));
        assert_eq!(e.effective_sizing("greedy").size_pct, dec!(20));
        assert!(e.effective_sizing("greedy").strategy_scoped);
        assert_eq!(e.entry_budget_usd("greedy"), dec!(20));
        assert_eq!(e.compute_shares(dec!(0.40), "greedy", "default"), dec!(50));
        assert_eq!(e.compute_shares(dec!(0.40), "timid", "default"), dec!(12)); // 5/0.4 = 12.5
        // A weight scales the leg's budget inside its own percentage.
        let mut c2 = pct_cfg(dec!(20), dec!(10), dec!(1000));
        c2.strategy_sizes = [("half".to_string(), over(dec!(20), Some(dec!(0.5))))].into();
        let mut e2 = engine_from(c2);
        e2.set_equity_usd(dec!(100));
        assert_eq!(e2.entry_budget_usd("half"), dec!(10));
        assert_eq!(e2.compute_shares(dec!(0.40), "half", "default"), dec!(25));
        // Global OFF is "no opinion", not a zero ceiling: a leg configured with
        // its own percentage is then the only one in force, and a leg without
        // one stays on the absolute path.
        let mut c3 = pct_cfg(Decimal::ZERO, dec!(10), dec!(1000));
        c3.strategy_sizes = [("only".to_string(), over(dec!(10), None))].into();
        let mut e3 = engine_from(c3);
        e3.set_equity_usd(dec!(100));
        assert_eq!(e3.entry_budget_usd("only"), dec!(10));
        assert_eq!(e3.compute_shares(dec!(0.40), "only", "default"), dec!(25));
        assert_eq!(e3.effective_sizing("unset").size_pct, Decimal::ZERO);
    }

    /// The shipped default must be untouched by this PR: with `size_pct = 0` the
    /// engine ignores the equity entirely, so the service pushing a balance once
    /// per cycle (as it now always does) cannot move one share of an
    /// unconfigured deployment's order flow.
    #[test]
    fn unconfigured_sizing_ignores_the_equity_entirely() {
        let mut e = engine_from(cfg()); // size_pct = 0, size_usd 2.5, band [10, 10]
        let before = e.compute_shares(dec!(0.25), "spread_arb", "default");
        assert_eq!(before, dec!(10));
        e.set_equity_usd(dec!(4.8));
        assert_eq!(
            e.compute_shares(dec!(0.25), "spread_arb", "default"),
            before
        );
        assert_eq!(
            e.entry_ticket(dec!(0.25), "spread_arb", "default"),
            Some(before)
        );
        assert_eq!(e.entry_budget_usd("spread_arb"), dec!(2.5));
        // Even a zero absolute budget keeps the historical zero-size order (that
        // path's own off switch) instead of turning into a silent skip.
        let mut off = cfg();
        off.size_usd = Decimal::ZERO;
        let off = engine_from(off);
        assert_eq!(
            off.compute_shares(dec!(0.25), "spread_arb", "default"),
            Decimal::ZERO
        );
        assert_eq!(
            off.entry_ticket(dec!(0.25), "spread_arb", "default"),
            Some(Decimal::ZERO)
        );
    }

    // ── E2-a per-strategy sizing ────────────────────────────────────────────

    fn sizing_cfg(overrides: &[(&str, StrategySize)]) -> EngineConfig {
        let mut c = cfg();
        c.size_usd = dec!(10);
        c.min_shares = dec!(2);
        c.max_shares = dec!(20);
        c.strategy_sizes = overrides
            .iter()
            .map(|(n, s)| ((*n).to_string(), s.clone()))
            .collect();
        c
    }

    #[test]
    fn strategy_sizing_falls_back_to_the_globals_when_absent() {
        let e = engine_from(sizing_cfg(&[]));
        // No entry at all → the global band, and reported as not strategy-scoped.
        let s = e.effective_sizing("whatever");
        assert_eq!(s.size_usd, dec!(10));
        assert_eq!(s.min_shares, dec!(2));
        assert_eq!(s.max_shares, dec!(20));
        assert!(!s.strategy_scoped);
        assert_eq!(
            e.compute_shares(dec!(0.50), "whatever", "default"),
            dec!(20)
        ); // 10/0.5=20

        // A caps-only entry (no sizing fields) must behave exactly like absent.
        let e2 = engine_from(sizing_cfg(&[("capped", StrategySize::default())]));
        assert_eq!(e2.effective_sizing("capped").size_usd, dec!(10));
        assert!(!e2.effective_sizing("capped").strategy_scoped);
    }

    #[test]
    fn strategy_sizing_overrides_take_effect_inside_the_global_band() {
        // Global band here is [2,20] on a 10u budget (see `sizing_cfg`).
        let e = engine_from(sizing_cfg(&[(
            "small",
            StrategySize {
                size_usd: Some(dec!(1)),
                min_shares: Some(dec!(1)),
                max_shares: Some(dec!(2)),
                size_weight: None,
                size_pct: None,
            },
        )]));
        let s = e.effective_sizing("small");
        // The 1-share floor is raised to the global floor (2): the global band
        // bounds the strategy from BELOW as well, so a lot can never fall under
        // the venue/risk minimum.
        assert_eq!(
            (s.size_usd, s.min_shares, s.max_shares),
            (dec!(1), dec!(2), dec!(2))
        );
        assert!(s.strategy_scoped);
        // 1 / 0.45 ≈ 2.2 → rounds to 2, inside [2,2].
        assert_eq!(e.compute_shares(dec!(0.45), "small", "default"), dec!(2));
        // 1 / 0.05 = 20 → clamped to the strategy's own 2-share cap.
        assert_eq!(e.compute_shares(dec!(0.05), "small", "default"), dec!(2));
        // 1 / 2.00 = 0.5 → rounds to 1, then raised to the global 2-share floor.
        assert_eq!(e.compute_shares(dec!(2.00), "small", "default"), dec!(2));
        // The un-configured sibling keeps using the globals in the same engine.
        assert_eq!(e.compute_shares(dec!(0.50), "other", "default"), dec!(20));
    }

    /// E16/#98 weighted allocation: a weight re-weights the leg's share of the
    /// per-entry budget and can never widen the global cap; a non-positive
    /// weight zeroes the leg out.
    #[test]
    fn strategy_size_weight_reweights_but_never_widens() {
        let e = engine_from(sizing_cfg(&[
            (
                "half",
                StrategySize {
                    size_weight: Some(dec!(0.5)),
                    ..Default::default()
                },
            ),
            (
                "greedy",
                StrategySize {
                    size_weight: Some(dec!(4)),
                    ..Default::default()
                },
            ),
            (
                "off",
                StrategySize {
                    size_weight: Some(Decimal::ZERO),
                    ..Default::default()
                },
            ),
        ]));
        // half: 10 × 0.5 = 5 → 5/0.5 = 10 shares (global band [2,20]).
        let h = e.effective_sizing("half");
        assert_eq!(h.size_usd, dec!(5));
        assert_eq!(h.size_weight, Some(dec!(0.5)));
        assert!(h.strategy_scoped);
        assert_eq!(e.compute_shares(dec!(0.50), "half", "default"), dec!(10));
        // greedy: 10 × 4 clamps back to the global 10 — the weight cannot
        // widen the budget.
        let g = e.effective_sizing("greedy");
        assert_eq!(g.size_usd, dec!(10));
        // off: weight 0 → no budget at all.
        assert_eq!(e.effective_sizing("off").size_usd, Decimal::ZERO);
        assert_eq!(e.compute_shares(dec!(0.50), "off", "default"), dec!(0));
    }

    #[test]
    fn strategy_sizing_can_never_exceed_the_global_risk_ceiling() {
        // An override that asks for MORE than the global band is clamped: more
        // notional, a higher share cap and a lower floor are all ignored.
        let e = engine_from(sizing_cfg(&[(
            "greedy",
            StrategySize {
                size_usd: Some(dec!(100)),
                min_shares: Some(dec!(0)),
                max_shares: Some(dec!(999)),
                size_weight: None,
                size_pct: None,
            },
        )]));
        let s = e.effective_sizing("greedy");
        assert_eq!(
            s.size_usd,
            dec!(10),
            "notional must be capped at the global budget"
        );
        assert_eq!(
            s.max_shares,
            dec!(20),
            "share ceiling must clamp to the global max"
        );
        assert_eq!(
            s.min_shares,
            dec!(2),
            "floor must not drop below the global min"
        );
        // 100/0.05 would be 2000 shares; the global ceiling still binds.
        assert_eq!(e.compute_shares(dec!(0.05), "greedy", "default"), dec!(20));
    }

    #[test]
    fn strategy_floor_above_the_global_ceiling_still_yields_the_ceiling() {
        // Degenerate config: floor 50 with a global ceiling of 20. The ceiling
        // wins (a floor is never allowed to defeat the global risk cap).
        let e = engine_from(sizing_cfg(&[(
            "weird",
            StrategySize {
                size_usd: None,
                min_shares: Some(dec!(50)),
                max_shares: None,
                size_weight: None,
                size_pct: None,
            },
        )]));
        let s = e.effective_sizing("weird");
        assert_eq!((s.min_shares, s.max_shares), (dec!(20), (dec!(20))));
        assert_eq!(e.compute_shares(dec!(0.10), "weird", "default"), dec!(20));
    }

    // ── #202 × declared shares: the equity cap reaches depth-sized legs ────

    /// Two strategies that DECLARE shares (the single_leg_pair shape), sized by
    /// `--size-pct` per strategy: the equity ticket caps the declaration, and
    /// the global cap still wins when it is tighter.
    #[test]
    fn an_armed_size_pct_caps_declared_shares_to_the_equity_ticket() {
        // Equity 100: `pct20` gets 20% = $20; at the dip's mid 0.44 the
        // whole-share ticket is floor(20 / 0.44) = 45 shares; `pct1` gets $1
        // → floor(1 / 0.44) = 2 shares.
        let mut e = engine_from(sizing_cfg(&[
            (
                "pct20",
                StrategySize {
                    size_pct: Some(dec!(20)),
                    ..Default::default()
                },
            ),
            (
                "pct1",
                StrategySize {
                    size_pct: Some(dec!(1)),
                    ..Default::default()
                },
            ),
        ]));
        e.set_equity_usd(dec!(100));
        e.register_user_strategy(
            Box::new(SharesBuyer::on_assets(
                "pct20",
                dec!(0.45),
                dec!(200),
                &["BTC"],
            )),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(
            Box::new(SharesBuyer::on_assets(
                "pct1",
                dec!(0.45),
                dec!(200),
                &["ETH"],
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("pct20", true));
        assert!(e.set_strategy_enabled("pct1", true));
        assert!(
            e.set_strategy_enabled("spread_arb", false),
            "isolate the two declared dips"
        );

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        for asset in ["BTC", "ETH"] {
            e.on_data(DataEvent::Book {
                token_id: format!("{}_up", asset.to_lowercase()),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now + 1_000,
            });
        }
        let orders = e.evaluate(now + 2_000);
        assert_eq!(orders.len(), 2, "{orders:?}");
        let big = orders
            .iter()
            .find(|o| o.strategy == "pct20")
            .expect("pct20 entry");
        let small = orders
            .iter()
            .find(|o| o.strategy == "pct1")
            .expect("pct1 entry");
        // Declared 200 is capped by the equity ticket (45), then by the
        // strategy's global-clamped share ceiling (20).
        assert_eq!(big.size, dec!(20), "{big:?}");
        assert_eq!(small.size, dec!(2), "{small:?}");
        assert_eq!(big.price, dec!(0.44));
        assert_eq!(small.price, dec!(0.44));
        assert_eq!(e.size_pct_skip_count(), 0);
    }

    /// With the mode ARMED, a declared count whose price class sits above the
    /// equity budget (ticket 0: the budget cannot buy one whole share) buys
    /// NOTHING and is counted — the same statement `entry_ticket` makes for
    /// the notional path (#202). A PAIR leg must never fall back to notional
    /// sizing: that would open the naked leg.
    #[test]
    fn a_declared_count_above_the_armed_budget_buys_nothing() {
        let mut c = sizing_cfg(&[(
            "tiny",
            StrategySize {
                size_pct: Some(dec!(0.1)),
                ..Default::default()
            },
        )]);
        // Global ceiling 20 must not resurrect the leg either.
        c.max_shares = dec!(20);
        let mut e = engine_from(c);
        e.set_equity_usd(dec!(1)); // 0.1% = $0.001 → floor(0.001/0.44) = 0
        e.register_user_strategy(
            Box::new(SharesBuyer::on_assets(
                "tiny",
                dec!(0.45),
                dec!(5),
                &["BTC"],
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("tiny", true));
        assert!(e.set_strategy_enabled("spread_arb", false));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_800_000)],
            now_ms: now,
        });
        e.on_data(DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        });
        let orders = e.evaluate(now + 2_000);
        assert!(
            orders.iter().all(|o| o.strategy != "tiny"),
            "a zero-ticket declaration must not emit: {orders:?}"
        );
        assert_eq!(e.size_pct_skip_count(), 1, "the skip is counted");
    }

    /// The cap is INERT while `size_pct` is off (the default): a declared
    /// count on the absolute path is still only bounded by `max_shares`, so
    /// every existing replay is untouched (bit-for-bit discipline).
    #[test]
    fn size_pct_off_leaves_declared_shares_clamped_only_by_max_shares() {
        let mut e = engine_from(sizing_cfg(&[]));
        e.set_equity_usd(dec!(100));
        e.register_user_strategy(
            Box::new(SharesBuyer::on_assets("BTC", dec!(0.45), dec!(7), &["BTC"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("BTC", true));
        assert!(e.set_strategy_enabled("spread_arb", false));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_800_000)],
            now_ms: now,
        });
        e.on_data(DataEvent::Book {
            token_id: "btc_up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 1_000,
        });
        let orders = e.evaluate(now + 2_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(
            orders[0].size,
            dec!(7),
            "declared 7 ≤ global cap 20: honoured"
        );
    }

    /// Per-asset market helper (the shared `market()` only makes BTC).
    fn market_of(asset: &str, end_ms: i64) -> CryptoMarket {
        let lower = asset.to_lowercase();
        CryptoMarket {
            asset: asset.into(),
            condition_id: format!("cond_{asset}"),
            question_id: format!("q_{asset}"),
            up_token_id: format!("{lower}_up"),
            down_token_id: format!("{lower}_down"),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: String::new(),
            neg_risk: true,
            question: format!("{asset} up or down"),
        }
    }

    /// A hosted strategy that DECLARES its share count on every signal (the
    /// `single_leg_pair` / `pair_discount_arb` shape): it sizes from the
    /// visible ask depth, which has nothing to do with the account. The
    /// declared amount is configurable so a test can place it above or below
    /// the equity ticket.
    struct SharesBuyer {
        name: String,
        account: String,
        buy_below: Decimal,
        declared: Decimal,
        assets: Vec<String>,
    }
    impl SharesBuyer {
        fn on_assets(name: &str, buy_below: Decimal, declared: Decimal, assets: &[&str]) -> Self {
            Self {
                name: name.to_string(),
                account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                buy_below,
                declared,
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
            }
        }
    }
    impl EngineStrategy for SharesBuyer {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(
            &mut self,
            _token_id: &str,
            _snap: &crate::model::OrderbookSnapshot,
            _now_ms: i64,
        ) {
        }
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
        fn gate_exemptions(&self) -> GateExemptions {
            GateExemptions::none()
        }
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            let mut out = Vec::new();
            for market in ctx.markets() {
                if !self.assets.is_empty() && !self.assets.contains(&market.asset) {
                    continue;
                }
                let Some(book) = ctx.fresh_book(&market.up_token_id) else {
                    continue;
                };
                if book.mid_price <= self.buy_below {
                    out.push(TradeSignal {
                        strategy: self.name.clone(),
                        account_id: self.account.clone(),
                        asset: market.asset.clone(),
                        direction: SignalDirection::Up,
                        token_id: market.up_token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price: book.mid_price,
                        reason: format!("{} depth dip", self.name),
                        shares: Some(self.declared),
                        pair: None,
                    });
                }
            }
            out
        }
    }

    #[test]
    fn two_strategies_size_independently_in_one_evaluation() {
        // One cycle, two strategies, one dip each on its OWN asset: every entry
        // carries the strategy's own lot instead of a single global size.
        let mut e = engine_from(sizing_cfg(&[
            (
                "small",
                StrategySize {
                    size_usd: Some(dec!(1)),
                    min_shares: Some(dec!(1)),
                    max_shares: Some(dec!(1)),
                    size_weight: None,
                    size_pct: None,
                },
            ),
            (
                "big",
                StrategySize {
                    size_usd: Some(dec!(10)),
                    min_shares: Some(dec!(20)),
                    max_shares: Some(dec!(20)),
                    size_weight: None,
                    size_pct: None,
                },
            ),
        ]));
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("small", dec!(0.45), &["BTC"])),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("big", dec!(0.45), &["ETH"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("small", true));
        assert!(e.set_strategy_enabled("big", true));
        assert!(
            e.set_strategy_enabled("spread_arb", false),
            "isolate the two sized dips"
        );

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        for asset in ["BTC", "ETH"] {
            e.on_data(DataEvent::Book {
                token_id: format!("{}_up", asset.to_lowercase()),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now + 1_000,
            });
        }
        let orders = e.evaluate(now + 2_000);
        assert_eq!(orders.len(), 2, "{orders:?}");
        let small = orders
            .iter()
            .find(|o| o.strategy == "small")
            .expect("small entry");
        let big = orders
            .iter()
            .find(|o| o.strategy == "big")
            .expect("big entry");
        assert_eq!(small.asset, "BTC");
        assert_eq!(big.asset, "ETH");
        assert_eq!(
            small.size,
            dec!(1),
            "1u / 0.44 rounds to 2, capped by its own 1-share lot"
        );
        assert_eq!(big.size, dec!(20));
        assert_eq!(small.price, dec!(0.44), "same dip price for both");
        assert_eq!(big.price, dec!(0.44));
    }

    // ── P-1.1 strategy dispatch ─────────────────────────────────────────────

    /// A trivial hosted strategy: buy either outcome token when its mid dips to
    /// or below a level. Implements the SAME full EngineStrategy contract an
    /// external v2 dylib does (external is only a loading difference).
    struct DipBuyer {
        name: String,
        account: String,
        buy_below: Decimal,
        /// Empty = every market. A non-empty list restricts the strategy to those
        /// assets, which is how one cycle can host several strategies without
        /// them competing for the same token.
        assets: Vec<String>,
        /// Shared entry gates this strategy declares it does not need (E2-b).
        gates: GateExemptions,
    }
    impl DipBuyer {
        fn new(name: &str, buy_below: Decimal) -> Self {
            Self {
                name: name.to_string(),
                account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                buy_below,
                assets: Vec::new(),
                gates: GateExemptions::none(),
            }
        }
        fn on_assets(name: &str, buy_below: Decimal, assets: &[&str]) -> Self {
            Self {
                name: name.to_string(),
                account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                buy_below,
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
                gates: GateExemptions::none(),
            }
        }
        /// Same as [`Self::new`] but declaring gate exemptions and restricted to
        /// the given assets (so it does not compete for the builtin's token under
        /// the one-entry-per-token rule).
        fn exempting(
            name: &str,
            buy_below: Decimal,
            assets: &[&str],
            gates: GateExemptions,
        ) -> Self {
            Self {
                name: name.to_string(),
                account: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
                buy_below,
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
                gates,
            }
        }
    }
    impl EngineStrategy for DipBuyer {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(
            &mut self,
            _token_id: &str,
            _snap: &crate::model::OrderbookSnapshot,
            _now_ms: i64,
        ) {
        }
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
        fn gate_exemptions(&self) -> GateExemptions {
            self.gates
        }
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            let mut out = Vec::new();
            for market in ctx.markets() {
                if !self.assets.is_empty() && !self.assets.contains(&market.asset) {
                    continue;
                }
                for (token_id, direction) in [
                    (&market.up_token_id, SignalDirection::Up),
                    (&market.down_token_id, SignalDirection::Down),
                ] {
                    let Some(book) = ctx.fresh_book(token_id) else {
                        continue;
                    };
                    if book.mid_price <= self.buy_below {
                        out.push(TradeSignal {
                            strategy: self.name.clone(),
                            account_id: self.account.clone(),
                            asset: market.asset.clone(),
                            direction,
                            token_id: token_id.clone(),
                            condition_id: market.condition_id.clone(),
                            price: book.mid_price,
                            reason: format!("{} dip", self.name),
                            shares: None,
                            pair: None,
                        });
                    }
                }
            }
            out
        }
    }

    #[test]
    fn a_fresh_engine_registers_nothing_and_only_the_host_can_add_strategies() {
        // PR-B hard switch: the kernel ships ZERO strategies, so a bare
        // `Engine::new` must come up empty. This is the assertion that keeps a
        // privileged in-tree code path from creeping back in.
        let bare = Engine::new(cfg());
        assert!(
            bare.supported_strategies().is_empty(),
            "the kernel must not register any strategy: {:?}",
            bare.supported_strategies()
        );
        assert!(bare.enabled_strategies().is_empty());
        assert_eq!(bare.strategy_source("spread_arb"), None);

        // The test host then registers three adapters in a defined order: it is
        // the tie-break when two strategies want the same token in the same
        // cycle (one entry per token per cycle), and `spread_arb` is first.
        let mut e = engine();
        assert_eq!(
            e.supported_strategies(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ]
        );
        assert_eq!(
            e.enabled_strategies(),
            vec!["spread_arb".to_string()],
            "the host enables the first adapter explicitly; the rest stay off"
        );
        assert_eq!(e.strategy_source("trend_follow"), Some("test"));
        assert!(
            !e.set_strategy_enabled("nope", true),
            "unknown name must not toggle"
        );
        assert!(e.set_strategy_enabled("trend_follow", true));
        assert_eq!(
            e.enabled_strategies(),
            vec!["spread_arb".to_string(), "trend_follow".to_string()]
        );
    }

    #[test]
    fn the_chase_leg_declares_no_gate_exemption_but_is_evolvable() {
        // E4-a / #30: entering WITH the move is exactly what the shared spot
        // momentum gate wants, so the chase leg opts out of nothing. (E4-b, the
        // counter-trend leg, is the one that needs `momentum: true`.)
        let e = engine();
        let ex = e
            .strategy_gate_exemptions("trend_follow")
            .expect("registered");
        assert_eq!(ex, GateExemptions::none());
        assert!(!ex.any());
        // The only declaration under the default registry is the E4-b fade leg.
        let declared = e.declared_gate_exemptions();
        assert_eq!(declared.len(), 1, "{declared:?}");
        assert_eq!(declared[0].0, "mean_reversion");
        // It IS evolvable, with a coherent declaration and a twin that builds —
        // a declaration without a factory would be inert.
        let s = e
            .strategy_refs()
            .into_iter()
            .find(|s| s.name() == "trend_follow")
            .expect("registered");
        let knobs = s.evolvable_knobs();
        assert_eq!(knobs.len(), 6);
        assert!(knobs.iter().all(|k| k.is_coherent()));
        assert!(s.shadow_factory().is_some());
    }

    /// A rising 1-cent-tick book: `bid` climbs `steps` cents from 0.50, the offer
    /// sits one tick above. Prices live on the real cent grid (mid `x.xx5`), which
    /// matters: a half-cent-offset book would let `round2` collapse the ask onto
    /// the mid and the chase leg refuses to "lift" an offer that is not above mid.
    fn rising_books(e: &mut Engine, token: &str, steps: i64, start_ms: i64) {
        for i in 0..=steps {
            let bid = dec!(0.50) + Decimal::from(i) / Decimal::ONE_HUNDRED;
            e.on_data(DataEvent::Book {
                token_id: token.into(),
                bids: vec![(bid, dec!(100))],
                asks: vec![(bid + dec!(0.01), dec!(100))],
                now_ms: start_ms + i * 1000,
            });
        }
    }

    #[test]
    fn the_chase_leg_trades_a_breakout_the_dip_buyer_would_not() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // A RISING up-token: bid 0.50 → 0.62 (mid 0.505 → 0.625, +24%).
        rising_books(&mut e, "up", 12, now);
        let at = now + 12_000;
        // Disabled → nothing, however good the setup looks.
        assert!(
            e.evaluate(at).is_empty(),
            "the chase leg must not trade while disabled"
        );
        assert!(e.set_strategy_enabled("trend_follow", true));
        let orders = e.evaluate(at);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "trend_follow");
        assert_eq!(orders[0].token_id, "up");
        assert_eq!(orders[0].direction, "up");
        // It LIFTS the offer: the entry is above the mid, the inverse of the dip
        // buyer's below-mid resting bid.
        let mid = dec!(0.625);
        assert_eq!(orders[0].price, dec!(0.63), "the lifted offer");
        assert!(
            orders[0].price > mid,
            "{} must be above the mid {mid}",
            orders[0].price
        );
        assert!(
            orders[0].internal_key.starts_with("trend_follow:BTC:up:"),
            "{}",
            orders[0].internal_key
        );
        // And it cleared the spot gate without an exemption: with no spot buffer
        // at all the gate is a pass, so a breach here would mean a declaration.
        assert!(
            e.last_exemptions().is_empty(),
            "the chase leg waived nothing"
        );
    }

    #[test]
    fn the_chase_leg_is_blocked_by_the_shared_spot_gate_when_spot_falls() {
        // The gate rejects a bet whose direction fights the spot move. A chase
        // entry is WITH the token's move, but when spot opposes it the candidate
        // must be stopped (it declares no exemption) — proving the new strategy
        // is subject to the same gates as the incumbent.
        let mut e = engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: 1_000_000,
        });
        assert!(e.set_strategy_enabled("trend_follow", true));
        // Spot falls: the momentum filter will refuse an UP bet.
        for i in 0..10 {
            e.on_data(DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60000) - Decimal::from(i) * dec!(10),
                now_ms: 1_000_000 + i * 1000,
            });
        }
        rising_books(&mut e, "up", 12, 1_000_000);
        let orders = e.evaluate(1_000_000 + 12_000);
        assert!(orders.is_empty(), "spot moved against the bet: {orders:?}");
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "trend_follow" && b.reason == BlockReason::Momentum),
            "{:?}",
            e.last_blocked()
        );
    }

    #[test]
    fn the_two_builtins_do_not_starve_each_other() {
        // Both strategies enabled, both presented with the setups they want on
        // DIFFERENT tokens: each gets its own entry in the same cycle, each
        // tagged with its own name, and neither is dropped.
        let mut e = engine();
        assert!(e.set_strategy_enabled("trend_follow", true));
        let now = 1_000_000i64;
        let mut m = market(1_800_000);
        m.down_token_id = "down".into();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![m.clone()],
            now_ms: now,
        });

        // "up" climbs (chase), "down" holds then dips (dip buy).
        for i in 0..=12 {
            let up_bid = dec!(0.50) + Decimal::from(i) / Decimal::ONE_HUNDRED;
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(up_bid, dec!(100))],
                asks: vec![(up_bid + dec!(0.01), dec!(100))],
                now_ms: now + i * 1000,
            });
            e.on_data(DataEvent::Book {
                token_id: "down".into(),
                bids: vec![(dec!(0.61), dec!(100))],
                asks: vec![(dec!(0.63), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        // Dip on "down": mid 0.44 → the dip buyer's resting bid is 0.43, inside its
        // 0.45 ceiling, while the chase leg wants the OTHER token and is unaffected.
        e.on_data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        let orders = e.evaluate(now + 12_000);
        assert_eq!(
            orders.len(),
            2,
            "one entry per strategy per token: {orders:?}"
        );
        assert!(
            orders
                .iter()
                .any(|o| o.strategy == "trend_follow" && o.token_id == "up"),
            "{orders:?}"
        );
        assert!(
            orders
                .iter()
                .any(|o| o.strategy == "spread_arb" && o.token_id == "down"),
            "{orders:?}"
        );
        // Neither strategy was blocked out of existence.
        assert!(e.last_blocked().is_empty(), "{:?}", e.last_blocked());
    }

    #[test]
    fn a_shared_token_is_settled_by_registration_order_not_by_starvation() {
        // When both want the SAME token in the SAME cycle the kernel emits one
        // entry (one entry per token per cycle) and the registered-first strategy
        // wins. This is the documented, deterministic tie-break — not a race.
        //
        // The default engine config cannot produce this collision at all: the dip
        // buyer's ceiling is 0.45 while the chase leg only looks above 0.55. So the
        // collision is constructed deliberately by raising the dip buyer's ceiling,
        // which is exactly the configuration where the tie-break has to be decided.
        let mut c = cfg();
        c.spread_arb.trend_max_entry_price = dec!(0.70);
        let mut e = engine_from(c);
        assert!(e.set_strategy_enabled("trend_follow", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // A book both want: "up" rising on the cent grid (the chase leg lifts the
        // 0.63 offer) while the mid 0.625 is still inside the dip buyer's raised
        // band (it would rest 0.61, below the mid).
        rising_books(&mut e, "up", 12, now);
        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "one entry per token per cycle: {orders:?}");
        assert_eq!(
            orders[0].strategy, "spread_arb",
            "the incumbent holds the tie-break"
        );
        assert!(
            orders[0].price < dec!(0.625),
            "and it rests below the mid: {}",
            orders[0].price
        );
    }

    /// Reports one break per call and counts the calls, so a test can tell "the
    /// hook ran" from "the host acted on what it returned" (#302).
    struct BreakReporter {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl EngineStrategy for BreakReporter {
        fn name(&self) -> &str {
            "break_reporter"
        }
        fn on_book(&mut self, _token_id: &str, _snap: &OrderbookSnapshot, _now_ms: i64) {}
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
        fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            vec![("up".to_string(), dec!(0.5))]
        }
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            Vec::new()
        }
    }

    #[test]
    fn a_disabled_strategy_contributes_no_breaks() {
        let mut e = engine();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        e.register_user_strategy(
            Box::new(BreakReporter {
                calls: std::sync::Arc::clone(&calls),
            }),
            "test".into(),
        )
        .unwrap();

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Several arms end in `drain_breaks`; count from here rather than
        // assuming how many ran before this event.
        let drained = calls.load(std::sync::atomic::Ordering::Relaxed);

        // Registered but off. The hook still runs — its library may be queueing
        // breaks internally and this call is what empties that queue — and
        // nothing it returns reaches the host.
        let broken = e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + 1_000,
        });
        assert!(
            broken.is_empty(),
            "a disabled strategy must not break another's bids, got {broken:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            drained + 1,
            "the hook is drained even while disabled, so its queue cannot grow"
        );

        // Enabled, the same break reaches the host.
        assert!(e.set_strategy_enabled("break_reporter", true));
        let broken = e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + 2_000,
        });
        assert_eq!(broken, vec![("up".to_string(), dec!(0.5))]);
    }

    #[test]
    fn user_strategy_registers_disabled_and_is_tagged_once_enabled() {
        let mut e = engine();
        let name = e
            .register_user_strategy(
                Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
                "test".into(),
            )
            .unwrap();
        assert_eq!(name, "dip_buyer");
        assert_eq!(
            e.supported_strategies(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string(),
                "dip_buyer".to_string(),
            ]
        );
        assert_eq!(
            e.enabled_strategies(),
            vec!["spread_arb".to_string()],
            "starts disabled"
        );
        assert_eq!(e.strategy_source("dip_buyer"), Some("test"));
        // Duplicate names are rejected outright.
        assert!(
            e.register_user_strategy(
                Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
                "test".into()
            )
            .is_err()
        );

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + 1_000,
        });
        // Disabled → no order even though the mid satisfies the strategy.
        assert!(e.evaluate(now + 1_000).is_empty());

        assert!(e.set_strategy_enabled("dip_buyer", true));
        let orders = e.evaluate(now + 2_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "dip_buyer");
        assert_eq!(orders[0].asset, "BTC");
        assert!(
            orders[0].internal_key.starts_with("dip_buyer:BTC:"),
            "{}",
            orders[0].internal_key
        );
    }

    /// Issue #205: the freshness budget is the entry gate, so the operator's value
    /// has to be the one that decides. One book, one decision time, several
    /// configs: refused at the compiled default, accepted *unchanged* once the
    /// budget is widened past the book's age. The book is deliberately older than
    /// any plausible default and well inside the knob's ceiling, so this is the
    /// exact case an operator reaching for the flag cares about.
    #[test]
    fn the_configured_staleness_budget_decides_whether_a_book_can_trade() {
        let now = 1_000_000i64;
        let book_ms = now - 30_000; // 30s old at decision time

        let entries = |budget_ms: i64| -> Vec<crate::model::OrderRequest> {
            let mut e = engine_from(EngineConfig {
                max_orderbook_stale_ms: budget_ms,
                ..cfg()
            });
            e.register_user_strategy(
                Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
                "test".into(),
            )
            .unwrap();
            assert!(e.set_strategy_enabled("dip_buyer", true));
            e.on_data(DataEvent::RoundMarkets {
                markets: vec![market(1_800_000)],
                now_ms: now,
            });
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: book_ms,
            });
            e.evaluate(now)
        };

        // Default budget → the 30s-old book is stale, and a stale book is not a
        // priceable book: the strategy never sees it, so nothing is placed.
        let default_budget = entries(DEFAULT_MAX_ORDERBOOK_STALE_MS);
        assert!(
            default_budget.is_empty(),
            "a {}-ms-old book must not trade under the {DEFAULT_MAX_ORDERBOOK_STALE_MS}ms default: {default_budget:?}",
            now - book_ms
        );
        // Widened budget → the very same book is fresh enough and trades.
        let wide = entries(60_000);
        assert_eq!(
            wide.len(),
            1,
            "a 60s budget must accept the same book: {wide:?}"
        );
        assert_eq!(wide[0].strategy, "dip_buyer");
        assert_eq!(wide[0].asset, "BTC");
        // `0` is the operator's explicit "check OFF": accepted however old.
        assert_eq!(
            entries(0).len(),
            1,
            "0 disables the freshness check, so the book is accepted"
        );
        // The age is the deciding variable, and it sits strictly between the two
        // budgets — otherwise this test would pass for the wrong reason.
        assert!(now - book_ms > DEFAULT_MAX_ORDERBOOK_STALE_MS);
        assert!(now - book_ms < 60_000);
    }

    /// The boundary of the freshness rule, at the one place that enforces it for
    /// entries: an age equal to the budget is still fresh, one millisecond past it
    /// is not, and `0` disables the age test without accepting an EMPTY book
    /// (there would be nothing to price off).
    #[test]
    fn the_freshness_budget_boundary_and_its_off_switch() {
        let now = 1_000_000i64;
        let mut books = HashMap::new();
        let mut b = LocalBook::new();
        b.apply_snapshot(
            &[(dec!(0.55), dec!(100))],
            &[(dec!(0.57), dec!(100))],
            now - 8_000,
        );
        books.insert("up".to_string(), b);

        assert!(
            fresh_book(&books, "up", now, 8_000).is_some(),
            "an age exactly at the budget is still fresh"
        );
        assert!(
            fresh_book(&books, "up", now, 7_999).is_none(),
            "one millisecond past the budget is stale"
        );
        assert!(
            fresh_book(&books, "up", now, 0).is_some(),
            "0 disables the age test"
        );
        assert!(
            fresh_book(&books, "up", now, -1).is_some(),
            "any non-positive value is the OFF switch"
        );

        // OFF is not "accept anything": an empty book is still unpricable.
        let mut empty = LocalBook::new();
        empty.apply_snapshot(&[], &[], now);
        books.insert("flat".to_string(), empty);
        assert!(fresh_book(&books, "flat", now, 0).is_none());
        assert!(fresh_book(&books, "flat", now, 8_000).is_none());
        // An unknown token has no book at all.
        assert!(fresh_book(&books, "nope", now, 0).is_none());
    }

    #[test]
    fn two_strategies_run_side_by_side_with_separate_tags() {
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::new("dip_buyer", dec!(0.45))),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("dip_buyer", true));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Confirm the UP trend over 12 samples (spread_arb needs it).
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        // UP dips → spread_arb candidate (and dip_buyer's too, on the same token).
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        // DOWN is cheap → only dip_buyer signals there (no trend confirmation).
        e.on_data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.41), dec!(100))],
            asks: vec![(dec!(0.43), dec!(100))],
            now_ms: now + 12_000,
        });

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 2, "one entry per token per cycle: {orders:?}");
        assert!(
            orders
                .iter()
                .any(|o| o.token_id == "up" && o.strategy == "spread_arb"),
            "{orders:?}"
        );
        assert!(
            orders
                .iter()
                .any(|o| o.token_id == "down" && o.strategy == "dip_buyer"),
            "{orders:?}"
        );
    }

    // ── E2-b per-strategy gate opt-out (#27) ────────────────────────────────

    /// Book a trend-confirming run on `token`, then dip it: the setup the
    /// builtin `spread_arb` needs to produce a candidate.
    fn trend_then_dip(e: &mut Engine, token: &str, now: i64) {
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
    }

    /// A single dip book on `token` at `at_ms`, with no trend run — the builtin
    /// stays idle, so only a strategy that needs no trend confirmation can
    /// signal here. Book it at the instant you evaluate: a stale book is not
    /// priceable at all (`fresh_book` returns None).
    fn dip_only(e: &mut Engine, token: &str, at_ms: i64) {
        e.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: at_ms,
        });
    }

    /// Push a spot price that fell over the momentum window (against an UP bet).
    fn spot_falls(e: &mut Engine, asset: &str, from: Decimal, to: Decimal, now: i64) {
        e.on_data(DataEvent::Spot {
            asset: asset.into(),
            price: from,
            now_ms: now,
        });
        e.on_data(DataEvent::Spot {
            asset: asset.into(),
            price: to,
            now_ms: now + 2_000,
        });
    }

    #[test]
    fn declared_timing_exemption_lets_a_candidate_through_the_window_gate() {
        // Timing gate forced shut by an absurd min_round_age; a strategy that
        // declares the timing window unnecessary still enters. The exempting
        // strategy trades ETH so it does not compete for the builtin's BTC token
        // (one entry per token per cycle is NOT waivable).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "window_fade",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("window_fade", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);

        // The builtin is still gated; the declaring strategy is exempt.
        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "window_fade");
        assert_eq!(orders[0].asset, "ETH");
        assert!(
            e.last_blocked().iter().any(|b| b.strategy == "spread_arb"),
            "builtin stays gated"
        );
        assert!(
            !e.last_blocked().iter().any(|b| b.strategy == "window_fade"),
            "exempted strategy must not appear as blocked"
        );
        // Auditable: the honoured exemption names the strategy, the gate and the
        // exact block that was waived.
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert_eq!(ex[0].strategy, "window_fade");
        assert_eq!(ex[0].gate, "timing");
        assert!(ex[0].detail.contains("Round too young"), "{}", ex[0].detail);
        let line = ex[0].audit_line();
        assert!(
            line.contains("本单因策略 window_fade 豁免门禁 timing"),
            "{line}"
        );
    }

    #[test]
    fn a_strategy_that_declares_nothing_is_still_gated() {
        // Identical setup, identical strategy body — only the declaration differs.
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("plain", dec!(0.45), &["ETH"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("plain", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);

        assert!(
            e.evaluate(now + 12_000).is_empty(),
            "undeclared strategy must stay gated"
        );
        assert!(
            e.last_exemptions().is_empty(),
            "nothing may be recorded as exempted"
        );
        assert!(
            e.last_blocked().iter().any(|b| b.strategy == "plain"),
            "{:?}",
            e.last_blocked()
        );
    }

    #[test]
    fn round_markets_declared_expiry_beats_the_wall_clock_grid() {
        // The venue's declared round end (expires_at_ms) must drive
        // `time_left_sec` — not the host's wall-clock slot grid. Here the grid
        // would put the round end at 1_800_000 (800s left at 1_010_000), while
        // the venue declares 1_900_000 (890s left). Without the wiring the
        // D-31 floor flaked on wall-clock position (the strategy-gate-check
        // CI failure); with it the clock follows the venue declaration.
        let mut e = engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_900_000)],
            now_ms: 1_000_000,
        });
        assert_eq!(
            e.scanner().round_state(1_010_000).time_left_sec,
            890,
            "the declared expiry, not the slot grid, is authoritative"
        );
    }

    #[test]
    fn d31_declared_floor_stops_the_timing_exemption_waiving_the_time_left_gate() {
        // D-31: `timing: true` used to waive the time-left gate outright, so an
        // exempt strategy could enter with the round nearly over — where the exit
        // policy fires on the next tick and the trade is a guaranteed zero-hold.
        // A declared floor excludes exactly that window. The GLOBAL gate is the
        // ordinary 180s (that is what actually produces the block); the two
        // strategies differ ONLY in the floor they declare, which is the point.
        let mut late_cfg = cfg();
        late_cfg.scanner.min_round_age_sec = 0;
        late_cfg.scanner.min_time_left_sec = 180;
        let mut e = engine_from(late_cfg);
        // ETH: declares a 180s floor → must NOT be waived at 172s left.
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "floored",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: Some(180),
                },
            )),
            "test".into(),
        )
        .unwrap();
        // SOL: declares the SAME `timing` exemption but a lower floor, so its
        // opt-out still reaches into the last 100s. Keeping both in one cycle is
        // the point: the declared floor, and only it, separates them.
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "lowfloor",
                dec!(0.45),
                &["SOL"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: Some(100),
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("floored", true));
        assert!(e.set_strategy_enabled("lowfloor", true));
        // XRP: declares `timing` with NO floor. That must mean "use the kernel's
        // 180s", not "no floor at all" — i.e. it is blocked exactly like `floored`.
        // This is the backwards-compatibility guarantee a pre-D-31 library relies
        // on when it emits only the two booleans.
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "intestate",
                dec!(0.45),
                &["XRP"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("intestate", true));

        // Round expires at 1_800_000 → exactly 172s left at `at`.
        let at = 1_800_000 - 172_000;
        let now = at - 12_000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![
                market(1_800_000),
                market_of("ETH", 1_800_000),
                market_of("SOL", 1_800_000),
                market_of("XRP", 1_800_000),
            ],
            now_ms: now,
        });
        dip_only(&mut e, "eth_up", at);
        dip_only(&mut e, "sol_up", at);
        dip_only(&mut e, "xrp_up", at);

        let orders = e.evaluate(at);
        assert_eq!(
            e.scanner().round_state(at).time_left_sec,
            172,
            "fixture drift"
        );
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(
            orders[0].strategy, "lowfloor",
            "only the lower declared floor may still enter this late"
        );
        assert!(
            e.last_exemptions().iter().all(|x| x.strategy == "lowfloor"),
            "only the lower declared floor may be recorded as exempted: {:?}",
            e.last_exemptions()
        );
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "floored" && b.reason == BlockReason::Timing),
            "the floored strategy must be timing-blocked: {:?}",
            e.last_blocked()
        );
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "intestate" && b.reason == BlockReason::Timing),
            "an undeclared floor means the kernel's, not none: {:?}",
            e.last_blocked()
        );
        assert!(
            e.last_exemptions().iter().all(|x| x.strategy != "floored"),
            "nothing may be recorded as exempted for the floored strategy"
        );

        // The floor narrows the exemption; below it, nothing about the young-round
        // case changed. Re-run the same strategy against a "too young" block: its
        // `time_left_sec` is large, so the 180s floor never conflicts.
        let mut young_cfg = cfg();
        young_cfg.scanner.min_round_age_sec = 10_000;
        young_cfg.scanner.min_time_left_sec = 180;
        let mut e = engine_from(young_cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "floored",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: Some(180),
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("floored", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        dip_only(&mut e, "eth_up", now + 12_000);
        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "too-young must stay waivable: {orders:?}");
        assert_eq!(orders[0].strategy, "floored");
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert!(
            ex[0].detail.contains("Round too young"),
            "the honoured exemption is the young-round one: {}",
            ex[0].detail
        );
    }

    #[test]
    fn d31_floor_is_read_through_the_abi_json_and_clamped_at_zero() {
        // The floor crosses the ABI as an optional JSON key, so it must survive a
        // round trip, degrade to the kernel default when absent, and clamp a
        // negative declaration (which would restore the unbounded waiver).
        let declared = serde_json::json!({
            "timing": true,
            "momentum": false,
            "timing_min_time_left_sec": 180,
        });
        let g = GateExemptions::from_json(&declared);
        assert!(g.timing && !g.momentum);
        assert_eq!(g.timing_min_time_left_sec, Some(180));
        assert_eq!(g.timing_floor_sec(0), 180);
        assert_eq!(g.to_json(), declared, "the declaration round-trips");

        // A pre-D-31 library emits only the two booleans: the missing key must
        // mean "use the kernel default", never "no floor".
        let legacy = GateExemptions::from_json(&serde_json::json!({
            "timing": true,
            "momentum": false,
        }));
        assert_eq!(legacy.timing_min_time_left_sec, None);
        assert_eq!(legacy.timing_floor_sec(180), 180);
        assert_eq!(
            legacy.to_json(),
            serde_json::json!({ "timing": true, "momentum": false }),
            "an undeclared floor must not be written back"
        );

        // Malformed / negative declarations degrade inward, never outward.
        let bad = GateExemptions::from_json(&serde_json::json!({
            "timing": true,
            "timing_min_time_left_sec": "180",
        }));
        assert_eq!(
            bad.timing_min_time_left_sec, None,
            "a string is not a floor"
        );
        assert_eq!(
            GateExemptions::from_json(&serde_json::json!({
                "timing": true,
                "timing_min_time_left_sec": -60,
            }))
            .timing_floor_sec(0),
            0,
            "a negative floor clamps to zero"
        );
    }

    #[test]
    fn declared_momentum_exemption_lets_a_mean_reversion_entry_through() {
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fader",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: false,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("fader", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);
        // Both spots fall over the momentum window: exactly the case a
        // mean-reversion entry wants (and the builtin filter rejects).
        spot_falls(&mut e, "BTC", dec!(60000), dec!(59000), now + 1000);
        spot_falls(&mut e, "ETH", dec!(3000), dec!(2950), now + 1000);

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "fader");
        assert_eq!(orders[0].asset, "ETH");
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert_eq!(ex[0].gate, "momentum");
        assert!(ex[0].detail.contains("spot ETH"), "{}", ex[0].detail);
        assert!(ex[0].detail.contains("vs up"), "{}", ex[0].detail);
        // The builtin, declaring nothing, was blocked by the very same filter.
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "spread_arb" && b.reason == BlockReason::Momentum)
        );
    }

    #[test]
    fn an_exemption_is_scoped_to_the_declaring_strategy_only() {
        // Two strategies on different assets in the SAME cycle: only the one that
        // declared the momentum exemption gets through.
        let mut e = engine();
        assert!(e.set_strategy_enabled("spread_arb", false));
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fader",
                dec!(0.45),
                &["BTC"],
                GateExemptions {
                    timing: false,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("fader", true));
        // A second, undeclared strategy restricted to ETH.
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("gated", dec!(0.45), &["ETH"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("gated", true));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        dip_only(&mut e, "up", now + 2_000);
        dip_only(&mut e, "eth_up", now + 2_000);
        spot_falls(&mut e, "BTC", dec!(60000), dec!(59000), now);
        spot_falls(&mut e, "ETH", dec!(3000), dec!(2950), now);

        let orders = e.evaluate(now + 2_000);
        assert_eq!(
            orders.len(),
            1,
            "only the declaring strategy may pass: {orders:?}"
        );
        assert_eq!(orders[0].strategy, "fader");
        assert_eq!(orders[0].asset, "BTC");
        assert_eq!(e.last_exemptions().len(), 1);
        assert!(e.last_blocked().iter().any(|b| b.strategy == "gated"
            && b.asset == "ETH"
            && b.reason == BlockReason::Momentum));
    }

    #[test]
    fn the_no_market_precondition_is_never_exemptible() {
        // Declaring every gate must not conjure a market: with no round markets
        // there is no priceable token, so nothing is exempted.
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "all_in",
                dec!(0.99),
                &[],
                GateExemptions::all(),
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("all_in", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        });
        assert!(
            e.evaluate(now).is_empty(),
            "no market → no candidates at all"
        );
        assert!(
            e.last_exemptions().is_empty(),
            "a structural precondition is not exemptible"
        );
    }

    #[test]
    fn blocked_candidates_carry_their_owning_strategy() {
        // Per-strategy attribution of blocked.timing / blocked.momentum (#27).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        assert!(e.evaluate(now + 12_000).is_empty());
        assert_eq!(e.last_blocked().len(), 1);
        assert_eq!(e.last_blocked()[0].strategy, "spread_arb");

        e.tally_blocked();
        assert_eq!(e.gate_tally("spread_arb").blocked_timing, 1);
        assert_eq!(e.gate_tally("spread_arb").exempted_timing, 0);
        assert_eq!(e.gate_tally("nobody"), StrategyGateTally::default());
    }

    /// 账户跟随（策略计数的账户切片）：same strategy on two accounts, each
    /// scope's gate verdicts land under ITS OWN (strategy, account) key —
    /// account A's page must never print account B's blocked counts, and the
    /// global per-name tally stays the exact sum (the replay baseline reads
    /// it byte-for-byte unchanged).
    #[test]
    fn blocked_gate_counts_are_attributable_per_account() {
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        let now = 1_000_000i64;
        // The paper scope is trading-OFF until the host mirrors the account
        // book (the engine fixture never does).
        e.set_account_trading("paper", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Two candidates for the SAME token, DIFFERENT accounts: the timing
        // gate blocks both (one entry per (account, token), so both ride the
        // batch), each stamped with its own scope.
        let orders = e.submit_candidate_orders_for_test(
            vec![
                scoped_signal(DEFAULT_ACCT, "up"),
                scoped_signal("paper", "up"),
            ],
            now + 12_000,
        );
        assert!(orders.is_empty(), "both scopes are gated: {orders:?}");
        assert_eq!(e.last_blocked().len(), 2);

        e.tally_blocked();
        let d = e.gate_tally_for_account("probe", DEFAULT_ACCT);
        let p = e.gate_tally_for_account("probe", "paper");
        assert_eq!(d.blocked_timing, 1, "{d:?}");
        assert_eq!(p.blocked_timing, 1, "{p:?}");
        assert_eq!(d.blocked_momentum, 0);
        assert_eq!(p.exempted_timing, 0);
        // A scope that never saw a gate verdict reads zeros.
        assert_eq!(
            e.gate_tally_for_account("probe", "nobody"),
            StrategyGateTally::default()
        );
        // The per-name fold is unchanged: the sum of the slices.
        assert_eq!(e.gate_tally("probe").blocked_timing, 2);
    }

    /// Same slicing for the honoured-exemption trail: each scope's exemption
    /// counts under its own (strategy, account) key, global unchanged.
    #[test]
    fn honoured_exemptions_are_counted_per_account() {
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "window_fade",
                dec!(0.45),
                &["BTC"],
                GateExemptions {
                    timing: true,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("window_fade", true));
        let now = 1_000_000i64;
        e.set_account_trading("paper", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        // Both scopes' candidates go through; both are EXEMPTED (the declared
        // opt-out), each stamped with its own account. The candidates carry
        // the strategy's own name — the exemption lookup reads the declared
        // opt-out per strategy.
        let mut a = scoped_signal(DEFAULT_ACCT, "up");
        let mut p = scoped_signal("paper", "up");
        a.strategy = "window_fade".into();
        p.strategy = "window_fade".into();
        let orders = e.submit_candidate_orders_for_test(vec![a, p], now + 12_000);
        assert_eq!(
            orders.len(),
            2,
            "both scopes ride the exemption: {orders:?}"
        );
        assert_eq!(e.last_exemptions().len(), 2);

        let drained = e.take_exemptions();
        assert_eq!(drained.len(), 2);
        assert!(
            drained
                .iter()
                .all(|r| r.strategy == "window_fade" && r.gate == "timing")
        );
        let d = e.gate_tally_for_account("window_fade", DEFAULT_ACCT);
        let p = e.gate_tally_for_account("window_fade", "paper");
        assert_eq!(d.exempted_timing, 1, "{d:?}");
        assert_eq!(p.exempted_timing, 1, "{p:?}");
        assert_eq!(d.blocked_timing, 0);
        assert_eq!(p.blocked_momentum, 0);
        // The per-name fold keeps its pre-fanout value semantics: the sum.
        assert_eq!(e.gate_tally("window_fade").exempted_timing, 2);
    }

    #[test]
    fn honoured_exemptions_are_counted_per_strategy_and_drained_once() {
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "window_fade",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("window_fade", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);
        assert_eq!(e.evaluate(now + 12_000).len(), 1);
        e.tally_blocked();

        let drained = e.take_exemptions();
        assert_eq!(drained.len(), 1);
        assert!(e.take_exemptions().is_empty(), "drained once");
        let t = e.gate_tally("window_fade");
        assert_eq!(t.exempted_timing, 1);
        assert_eq!(t.exempted_momentum, 0);
        // Counting an exemption never inflates the blocked counters: the builtin
        // was blocked by the very gate the other strategy declared unnecessary.
        assert_eq!(e.blocked_timing_count(), 1, "only the builtin was blocked");
        assert_eq!(e.gate_tally("spread_arb").blocked_timing, 1);
        assert_eq!(e.gate_tally("window_fade").blocked_timing, 0);
    }

    #[test]
    fn declared_exemptions_are_listed_for_audit() {
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fader",
                dec!(0.45),
                &[],
                GateExemptions {
                    timing: false,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(Box::new(DipBuyer::new("plain", dec!(0.45))), "test".into())
            .unwrap();
        let declared = e.declared_gate_exemptions();
        assert_eq!(
            declared.len(),
            2,
            "silent strategies are omitted: {declared:?}"
        );
        assert_eq!(
            declared[0].0, "mean_reversion",
            "the builtin declares first (hosted first)"
        );
        assert_eq!(declared[0].1.gates(), vec!["momentum"]);
        assert_eq!(declared[1].0, "fader");
        assert_eq!(declared[1].1.gates(), vec!["momentum"]);
        assert_eq!(
            e.strategy_gate_exemptions("plain"),
            Some(GateExemptions::none())
        );
        assert_eq!(e.strategy_gate_exemptions("nobody"), None);
    }

    #[test]
    fn the_builtin_declares_no_exemptions() {
        let e = engine();
        assert_eq!(
            e.strategy_gate_exemptions("spread_arb"),
            Some(GateExemptions::none())
        );
    }

    #[test]
    fn disabling_spread_arb_stops_its_entries() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        assert_eq!(e.evaluate(now + 12_000).len(), 1);
        assert!(e.set_strategy_enabled("spread_arb", false));
        // Same round, no order noted as pending: the empty result must come from
        // the disable, not from suppression.
        assert!(
            e.evaluate(now + 12_001).is_empty(),
            "disabled strategy must not emit"
        );
        assert!(e.set_strategy_enabled("spread_arb", true));
        assert_eq!(
            e.evaluate(now + 12_002).len(),
            1,
            "re-enabling restores entries"
        );
    }

    // ── 多账户扇出（多账户同时跑策略）：每条先红后绿的有牙测试 ────────────
    //
    // 部署形状：每（账户×策略）一个实例；default scope 是兼容形状（回放
    // 基线逐位不变的锚）。这里的测试用 `probe` 适配器（test_support 宿主
    // 注册的）+ 直接构造 TradeSignal 走 submit 路径。

    use blitzkrieg_market_api::DEFAULT_ACCOUNT_ID as DEFAULT_ACCT;

    /// A signal aimed at an ACCOUNT scope.
    fn scoped_signal(account: &str, token: &str) -> TradeSignal {
        TradeSignal {
            strategy: "probe".into(),
            account_id: account.to_string(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: token.into(),
            condition_id: "cond".into(),
            price: dec!(0.42),
            reason: "multi-account test".into(),
            shares: None,
            pair: None,
        }
    }

    /// Account A's entry candidate does not collide with account B's on the
    /// SAME token: the OME's global internal-key dedup disambiguates by the
    /// trailing account segment. (Today-red → the second order was dropped
    /// as a duplicate.)
    #[test]
    fn two_accounts_same_token_get_distinct_internal_keys() {
        let mut e = engine();
        let now = 1_000_000i64;
        // Host pushes each account book's facts before evaluate; the engine
        // fixture never does, so the tests state them the way the host would.
        e.set_account_trading("paper", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        let mut orders = e.submit_candidate_orders_for_test(
            vec![
                scoped_signal(DEFAULT_ACCT, "tok"),
                scoped_signal("paper", "tok"),
            ],
            now,
        );
        assert_eq!(orders.len(), 2, "both scopes emit");
        let k0 = orders.remove(0).internal_key;
        let k1 = orders.remove(0).internal_key;
        assert_ne!(k0, k1, "the account segment disambiguates");
        // The default scope's key stays byte-identical to the pre-fanout
        // spelling (4 segments, NO account segment) — the replay baseline
        // compares this exact string.
        assert!(
            !k0.contains(&format!(":{DEFAULT_ACCT}")),
            "default scope key must not carry an account segment: {k0}"
        );
        assert!(
            k1.ends_with(":paper"),
            "non-default scope key ends with the account segment: {k1}"
        );
    }

    /// A trading-closed scope contributes NO entry candidates; its exit
    /// intents keep flowing (「关=拒开仓、放平仓」).
    #[test]
    fn trading_closed_scope_drops_entries_exits_flow() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.set_account_trading("paper", false);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert!(orders.is_empty(), "closed scope's entry is dropped");
        // Re-open: the same candidate flows again.
        e.set_account_trading("paper", true);
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert_eq!(orders.len(), 1, "re-opened scope emits");
    }

    /// The account-scoped kill switch (PR G): a FROZEN scope contributes no
    /// entry candidates — exactly the trading-OFF shape — while a sibling
    /// scope on the same token keeps emitting (the isolation that makes the
    /// per-account freeze worth having). The default scope is freezable too:
    /// unlike `scope_trading`, there is no privileged default arm.
    #[test]
    fn frozen_scope_drops_entries_sibling_flows() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.set_account_trading("paper", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        e.set_account_frozen("paper", true);
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert!(orders.is_empty(), "frozen scope's entry is dropped");
        // The sibling on the SAME token is untouched by paper's freeze.
        e.set_account_trading("default", true);
        let orders = e.submit_candidate_orders_for_test(
            vec![
                scoped_signal("paper", "tok"),
                scoped_signal(DEFAULT_ACCT, "tok"),
            ],
            now,
        );
        assert_eq!(
            orders.len(),
            1,
            "only the non-frozen scope's candidate survives: {orders:?}"
        );
        // Lifting the freeze re-opens the scope.
        e.set_account_frozen("paper", false);
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert_eq!(orders.len(), 1, "resumed scope emits again");
        // The default scope freezes like any other (manual account.freeze
        // default must stop it too).
        e.set_account_frozen(DEFAULT_ACCT, true);
        let orders =
            e.submit_candidate_orders_for_test(vec![scoped_signal(DEFAULT_ACCT, "tok")], now);
        assert!(orders.is_empty(), "the default scope is freezable");
    }

    /// pending_tokens keys on (account, token): A's resting entry does not
    /// suppress B's candidate for the same token. (Today-red: the shared
    /// token key swallowed B.)
    #[test]
    fn pending_token_is_per_account() {
        let mut e = engine();
        e.note_scoped_order_placed("acct_a", "tok");
        // Same token, different account: not suppressed.
        let mut e2 = engine();
        e2.note_scoped_order_placed("acct_b", "tok");
        assert!(
            !e2.pending_tokens
                .contains(&("acct_a".to_string(), "tok".into())),
            "B's book never saw A's pending token"
        );
        // Same account, same token: suppressed (the dedup that matters).
        assert!(
            e2.pending_tokens
                .contains(&("acct_b".to_string(), "tok".into()))
        );
        // The default scope's key shape: ("default", token) — the historical
        // single-string key's exact behaviour.
        let mut e3 = engine();
        e3.note_order_placed("tok");
        assert!(
            e3.pending_tokens
                .contains(&(DEFAULT_ACCT.to_string(), "tok".into())),
            "default scope pending key = (default, token)"
        );
    }

    // ── 账户↔venue 解耦：per-account ENABLEMENT 语义牙（先红后绿） ─────────

    /// 双账户同策略同 token：A 停用 B 启用 → 只有 B scope 出意图。
    ///
    /// Scope enablement is per-INSTANCE: disabling (strategy, paper) — the
    /// write's rebuild omitting the name — never touches (strategy, paper2).
    /// The disable rides the rebuild: a FLAT scope drops outright; a HELD
    /// scope is demoted exits-only (its candidates die at the submission
    /// gate) while the sibling on the SAME strategy and token keeps
    /// emitting, and re-adding the name restores A independently.
    #[test]
    fn per_account_disable_leaves_sibling_scopes_trading() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.set_account_trading("paper", true);
        e.set_account_trading("paper2", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Instantiate both scopes the way the boot/rebuild does, then disable
        // A while it HOLDS (the realistic disable: the write's rebuild omits
        // the name and hands the held set — DEMOTE, not teardown).
        e.rebuild_account_scopes("paper", &["probe".into()], &[], &mut |n, a| {
            Some(probe_rebuild(n, a))
        });
        e.rebuild_account_scopes("paper2", &["probe".into()], &[], &mut |n, a| {
            Some(probe_rebuild(n, a))
        });
        e.rebuild_account_scopes("paper", &[], &["probe".into()], &mut |n, a| {
            Some(probe_rebuild(n, a))
        });
        assert!(
            e.strategies
                .iter()
                .any(|s| s.account_id == "paper" && s.exits_only),
            "the disabled-and-held scope is demoted exits-only"
        );

        // A disabled scope emits nothing…
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert!(orders.is_empty(), "disabled scope's entry is dropped");
        // …while the sibling scope of the SAME strategy+token keeps flowing.
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper2", "tok")], now);
        assert_eq!(
            orders.len(),
            1,
            "the sibling scope is untouched by paper's disable"
        );
        // Both candidates in one cycle: only the enabled scope's lands.
        let orders = e.submit_candidate_orders_for_test(
            vec![
                scoped_signal("paper", "tok"),
                scoped_signal("paper2", "tok"),
            ],
            now,
        );
        assert_eq!(orders.len(), 1, "only B's intent survives: {orders:?}");
        assert_eq!(orders[0].account_id.as_str(), "paper2");
        // Re-enabling restores A independently (the ADD phase un-demotes).
        e.rebuild_account_scopes("paper", &["probe".into()], &[], &mut |n, a| {
            Some(probe_rebuild(n, a))
        });
        assert!(
            !e.strategies
                .iter()
                .any(|s| s.account_id == "paper" && s.exits_only),
            "a scope back in the enablement set returns to normal"
        );
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert_eq!(orders.len(), 1, "re-enabled scope emits again");
    }

    /// 停用有持仓的 scope → 无新 entry、exit intent 照常流出（exits-only
    /// 降级）。The disable rides the rebuild's DEMOTE phase (the same
    /// mechanism a removed-and-held strategy gets): the scope stays hosted
    /// and `enabled`, so its exit intents KEEP being drained by evaluate —
    /// but its entry candidates die at the submission gate. Never a
    /// teardown, never a stuck position.
    #[test]
    fn disabling_a_scope_with_positions_demotes_exits_only() {
        struct ExitEmitter {
            name: String,
        }
        impl EngineStrategy for ExitEmitter {
            fn name(&self) -> &str {
                &self.name
            }
            fn on_book(&mut self, _: &str, _: &OrderbookSnapshot, _: i64) {}
            fn on_round(&mut self, _: i64, _: i64, _: i64) {}
            fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
                // Emit an entry for each held position's token — the shape a
                // strategy that wants to (re)enter under itself produces.
                ctx.held_positions()
                    .iter()
                    .map(|p| TradeSignal {
                        strategy: self.name.clone(),
                        account_id: "paper".into(),
                        asset: "BTC".into(),
                        direction: SignalDirection::Up,
                        token_id: p.token_id.clone(),
                        condition_id: "cond".into(),
                        price: dec!(0.42),
                        reason: "exit-only probe".into(),
                        shares: None,
                        pair: None,
                    })
                    .collect()
            }
            fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
                // One standing exit intent: the position must keep being
                // managed even after the scope is disabled.
                vec![StrategyExitIntent {
                    token_id: "held_tok".into(),
                    reason: "manual (exit-only probe)".into(),
                    account_id: "paper".into(),
                }]
            }
        }
        let mut e = Engine::new(cfg());
        // The paper scope hosts the emitter itself (a rebuild's build fn
        // would stand in a bare NamedStub — this scope must EMIT).
        e.register_scoped_strategy(
            Box::new(ExitEmitter {
                name: "probe".into(),
            }),
            "test".into(),
            "paper",
        )
        .expect("exit emitter registers");
        // Registration starts DISABLED; being in the account's enablement set
        // is what turns it on (the write's rebuild ENABLEs what lands).
        for s in &mut e.strategies {
            if s.account_id == "paper" {
                s.enabled = true;
            }
        }
        // The enablement write's shape: the scope is in the set (the rebuild
        // finds the existing scope and clears any demotion — the instance
        // with the exit emitter survives).
        e.rebuild_account_scopes("paper", &["probe".into()], &[], &mut |n, a| {
            Some(probe_rebuild(n, a))
        });
        // The host pushes the held positions BEFORE the evaluate (the same
        // push run_cycle does). HeldPosition's wire-facing string fields.
        let held_view = || {
            vec![HeldPosition {
                strategy: "probe".into(),
                account_id: "paper".into(),
                condition_id: "cond".into(),
                token_id: "held_tok".into(),
                direction: "up".into(),
                shares: "10".into(),
                entry_price: "0.40".into(),
                round_slot: 1,
            }]
        };
        e.set_positions_view(held_view());
        e.set_account_trading("paper", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: 1_000_000i64,
        });
        // Sanity while enabled: entries AND exits both flow.
        let orders = e.evaluate(1_000_000i64);
        assert_eq!(
            orders.len(),
            1,
            "enabled scope emits the held token's re-entry"
        );
        assert_eq!(
            e.drain_strategy_exits().len(),
            1,
            "enabled scope emits exits"
        );

        // Disable the scope WHILE HOLDING: the write's rebuild omits the
        // name and names what is held → DEMOTE.
        e.set_positions_view(held_view());
        let kept = e.rebuild_account_scopes("paper", &[], &["probe".into()], &mut |n, a| {
            Some(probe_rebuild(n, a))
        });
        assert_eq!(kept, vec!["probe".to_string()], "the demotion is reported");
        let orders = e.evaluate(1_000_001i64);
        assert!(orders.is_empty(), "disabled scope: no new entries");
        assert_eq!(
            e.drain_strategy_exits().len(),
            1,
            "exits-only demotion: exit intents keep flowing"
        );
        assert!(
            e.strategies
                .iter()
                .any(|s| s.account_id == "paper" && s.strategy.name() == "probe"),
            "a held scope is never torn down by the disable"
        );
    }

    /// Sizing reads the scope's OWN pushed equity, not the global scalar.
    #[test]
    fn sizing_uses_scope_equity_not_the_global_scalar() {
        let mut e = engine_from(pct_cfg(dec!(100), dec!(1), dec!(1000)));
        // The GLOBAL scalar (the active ledger's mirror) says $10; the
        // "rich" scope's own book says $500. At price 0.50:
        //   scalar sizing → 10/0.50 = 20 shares; scope sizing → 1000.
        e.set_equity_usd(dec!(10));
        e.set_account_equity("rich", dec!(500));
        let shares = e.compute_shares(dec!(0.50), "probe", "rich");
        assert_eq!(shares, dec!(1000), "the scope's own equity drives sizing");
        // The default scope keeps reading the global scalar.
        let default_shares = e.compute_shares(dec!(0.50), "probe", DEFAULT_ACCT);
        assert_eq!(default_shares, dec!(20), "default scope = the scalar");
        // A scope with NO push sizes against ZERO: fail-closed, no ticket.
        let none = e.entry_ticket(dec!(0.50), "probe", "ghost");
        assert_eq!(none, None, "unknown scope = zero equity = no ticket");
    }

    /// A venue-bound scope drops candidates whose token belongs to a
    /// DIFFERENT venue's round market — in the engine, before any order
    /// exists, so no place_gated cooldown burns.
    #[test]
    fn venue_bound_scope_drops_foreign_venue_candidates() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.set_account_venue("poly_only", "polymarket");
        e.set_account_trading("poly_only", true);
        e.set_account_trading("free", true);
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)], // the fixture market's venue
            now_ms: now,
        });
        let orders =
            e.submit_candidate_orders_for_test(vec![scoped_signal("poly_only", "tok")], now);
        assert!(
            orders.is_empty(),
            "a venue-bound scope never reaches into another venue's book"
        );
        // Unbound scope: the same candidate flows.
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("free", "tok")], now);
        assert_eq!(orders.len(), 1, "unbound scope trades any venue");
    }

    /// The exits-only demotion (account.strategies.set safety net): a
    /// removed-but-held strategy's scope stops contributing entries while
    /// its exit intents keep flowing; the flag clears when the strategy
    /// returns to the assignment.
    #[test]
    fn exits_only_scope_drops_entries_keeps_exits_flagged() {
        let mut e = engine();
        // Push the book fact so the empty assertion below can ONLY come from
        // the exits-only gate (an unpushed scope would fail closed anyway).
        e.set_account_trading("paper", true);
        e.rebuild_account_scopes(
            "paper",
            &["probe".to_string()],
            &[],
            &mut |name, account| Some(probe_rebuild(name, account)),
        );
        // probe now hosted for paper. Demote it exits-only (as if removed
        // while holding positions).
        for s in &mut e.strategies {
            if s.account_id == "paper" {
                s.exits_only = true;
            }
        }
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        let orders = e.submit_candidate_orders_for_test(vec![scoped_signal("paper", "tok")], now);
        assert!(orders.is_empty(), "exits-only scope: no entries");
        // Restore: the flag flips off in the rebuild, entries flow.
        e.rebuild_account_scopes(
            "paper",
            &["probe".to_string()],
            &[],
            &mut |name, account| Some(probe_rebuild(name, account)),
        );
        assert!(
            !e.strategies
                .iter()
                .any(|s| s.account_id == "paper" && s.exits_only),
            "a scope back in the assignment returns to normal"
        );
    }

    /// rebuild_account_scopes drops a removed-and-FLAT scope outright.
    #[test]
    fn rebuild_drops_removed_and_flat_scopes() {
        let mut e = engine();
        e.rebuild_account_scopes(
            "paper",
            &["probe".to_string()],
            &[],
            &mut |name, account| Some(probe_rebuild(name, account)),
        );
        assert_eq!(
            e.strategies
                .iter()
                .filter(|s| s.account_id == "paper")
                .count(),
            1,
            "the requested scope exists"
        );
        // A rebuild WITHOUT the name (and nothing held) drops it.
        e.rebuild_account_scopes("paper", &[], &[], &mut |name, account| {
            Some(probe_rebuild(name, account))
        });
        assert!(
            !e.strategies.iter().any(|s| s.account_id == "paper"),
            "removed-and-flat scope is gone"
        );
        // A rebuild that KEEPS the name but the build fn fails (non-Lua
        // source) still leaves the existing scope alone.
        e.rebuild_account_scopes("paper", &["probe".to_string()], &[], &mut |_, _| None);
    }

    /// Test-only seam: submit_candidate_orders is private; the multi-account
    /// tests drive it directly. Production paths (evaluate / replay) keep
    /// the private call.
    impl Engine {
        fn submit_candidate_orders_for_test(
            &mut self,
            candidates: Vec<TradeSignal>,
            now_ms: i64,
        ) -> Vec<crate::model::OrderRequest> {
            self.submit_candidate_orders(candidates, now_ms)
        }
    }

    /// A minimal named EngineStrategy stand-in for the scope-rebuild tests:
    /// the real builder is the service's Lua re-loader; the engine tests only
    /// need A strategy with the requested name (never traded here).
    struct NamedStub {
        name: String,
    }
    impl EngineStrategy for NamedStub {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _n: i64) {}
        fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            Vec::new()
        }
    }
    fn probe_rebuild(name: &str, _account: &str) -> Box<dyn EngineStrategy> {
        Box::new(NamedStub {
            name: name.to_string(),
        })
    }

    /// The assignment write is a DECISION: an ENABLED default scope the
    /// request omits (and nothing holds) is torn down outright — with
    /// assignments explicit there is no INHERIT resolution to protect, and
    /// the operator who trimmed the list asked for exactly this. Durable
    /// exposure rides the DEMOTE phase (held strategies keep managing their
    /// exits until flat).
    #[test]
    fn rebuild_drops_an_enabled_scope_the_assignment_omits() {
        let mut e = engine();
        e.rebuild_account_scopes(
            DEFAULT_ACCT,
            &["alpha".into(), "beta".into(), "gamma".into()],
            &[],
            &mut |name, _| Some(probe_rebuild(name, "default")),
        );
        assert!(e.set_strategy_enabled("alpha", true));
        // The explicit trim omits alpha (enabled) and gamma (disabled).
        e.rebuild_account_scopes(DEFAULT_ACCT, &["beta".into()], &[], &mut |name, _| {
            Some(probe_rebuild(name, "default"))
        });
        assert!(
            !e.strategies.iter().any(|s| s.strategy.name() == "alpha"),
            "an enabled scope the assignment omits drops — the trim is a decision"
        );
        assert!(
            !e.strategies.iter().any(|s| s.strategy.name() == "gamma"),
            "the disabled omitted scope drops as before"
        );
        assert!(
            e.strategies
                .iter()
                .any(|s| s.strategy.name() == "beta" && s.enabled),
            "the named scope arrives enabled (assignment IS enablement)"
        );
    }
}
