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
use crate::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
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
    /// The trend knobs' in-memory carrier. RFC-645 A-2 retired the concrete
    /// types from the STRATEGY face (`EngineStrategy::on_config` is opaque
    /// JSON now) — the field stays: the kernel's own config mapping
    /// (`CoreConfig::engine_config`) needs a typed home for the builtin
    /// strategies' knobs, and [`engine_strategy_config_json`] is the JSON
    /// projection strategies actually receive.
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

/// The strategy config push's JSON projection (RFC-645 A-2, 决议④): the
/// kernel-side marshal that used to live in `ForeignStrategy::on_config`
/// (strategies/foreign.rs), now the ONE sink — every hosted strategy, builtin
/// adapter or foreign library alike, receives the SAME bytes through
/// [`EngineStrategy::on_config`].
///
/// GOLDEN CONTRACT (决议④, wire 逐位不变): the camelCase spelling and the
/// decimal-as-STRING shapes below are the dylib wire contract the foreign
/// marshal layer has shipped for the whole life of the v2 ABI — pinned
/// byte-for-byte by `engine_strategy_config_json_is_the_foreign_marshal_golden`
/// (strategies/test_support.rs). Drift here is a wire break, not a refactor.
pub fn engine_strategy_config_json(
    trend: &TrendConfig,
    spread_arb: &SpreadArbConfig,
) -> serde_json::Value {
    serde_json::json!({
        "trend": {
            "confirmSec": trend.confirm_sec,
            "minPrice": trend.min_price.to_string(),
            "brokenPrice": trend.broken_price.to_string(),
            "ratio": trend.ratio.to_string(),
            "windowFloorMs": trend.window_floor_ms,
        },
        "spreadArb": {
            "trendMinPrice": spread_arb.trend_min_price.to_string(),
            "trendConfirmSec": spread_arb.trend_confirm_sec,
            "trendBrokenPrice": spread_arb.trend_broken_price.to_string(),
            "trendEntryPrice": spread_arb.trend_entry_price.to_string(),
            "trendEntryFactor": spread_arb.trend_entry_factor.to_string(),
            "trendMaxEntryPrice": spread_arb.trend_max_entry_price.to_string(),
            "entryMinObi": spread_arb.entry_min_obi.to_string(),
            "entryMaxSpreadPct": spread_arb.entry_max_spread_pct.to_string(),
            "entryDipMaxPct": spread_arb.entry_dip_max_pct.to_string(),
            "entryBounceMinPct": spread_arb.entry_bounce_min_pct.to_string(),
            "entryBounceWindowSec": spread_arb.entry_bounce_window_sec,
        }
    })
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
///
/// #648 C: the DEFINITION moved verbatim to `blitzkrieg_data_model` (the leaf
/// the offline `tools/onchain_pull` crate also compiles against); this
/// re-export keeps every `crate::engine::CashflowKind` spelling compiling.
pub use blitzkrieg_data_model::CashflowKind;

/// One book of a batch: `(token_id, bids, asks)` — the shape
/// [`DataEvent::Books`] and [`Engine::on_data_books`] both carry, named so the
/// signatures stay readable.
///
/// #648 C: definition lives in `blitzkrieg_data_model`; re-exported here so
/// the kernel's import paths stay stable.
pub use blitzkrieg_data_model::BatchBook;

/// Market-data events the engine consumes (produced by the feed layer or, in
/// tests, driven directly).
///
/// #648 C: definition lives in `blitzkrieg_data_model` — the same taxonomy the
/// archive layer encodes and the offline on-chain puller produces — and is
/// re-exported here so every `crate::engine::DataEvent` spelling compiles
/// unchanged.
pub use blitzkrieg_data_model::DataEvent;

mod evaluate;
mod submit;

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
    hot_params: Option<std::sync::Arc<crate::registry::ParamRegistry>>,
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
        // #779: a tiny positive price passes the `<= ZERO` guard but makes
        // `size_usd / price` overflow (`Decimal::div` PANICS). A ticket that
        // large is nonsense — emit no order.
        let raw = match sizing.size_usd.checked_div(price) {
            Some(raw) => raw.round(),
            None => {
                tracing::warn!(
                    strategy = %strategy,
                    account = %account,
                    size_usd = %sizing.size_usd,
                    price = %price,
                    "sizing overflow: size_usd / price does not fit Decimal — no ticket (#779)"
                );
                return Decimal::ZERO;
            }
        };
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
        // #779: every term below is guarded — an out-of-band equity, percentage
        // or price must not overflow `Decimal::mul`/`div` (both PANIC). Any
        // overflow means "no ticket", never a crash on the sizing path.
        let Some(pct_notional) = equity.checked_mul(sizing.size_pct) else {
            tracing::warn!(account = %account, equity = %equity, pct = %sizing.size_pct,
                "equity sizing overflow: equity * size_pct does not fit Decimal — no ticket (#779)");
            return Decimal::ZERO;
        };
        let Some(raw) = pct_notional.checked_div(Decimal::ONE_HUNDRED) else {
            return Decimal::ZERO;
        };
        let budget = match sizing.size_weight {
            Some(w) => {
                let Some(weighted) = raw.checked_mul(w) else {
                    tracing::warn!(account = %account, budget = %raw, weight = %w,
                        "equity sizing overflow: budget * weight does not fit Decimal — no ticket (#779)");
                    return Decimal::ZERO;
                };
                weighted.max(Decimal::ZERO).min(raw)
            }
            None => raw,
        };
        if budget <= Decimal::ZERO || sizing.max_shares <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let Some(ticket) = budget.checked_div(price) else {
            tracing::warn!(account = %account, budget = %budget, price = %price,
                "equity sizing overflow: budget / price does not fit Decimal — no ticket (#779)");
            return Decimal::ZERO;
        };
        ticket.floor().max(Decimal::ZERO).min(sizing.max_shares)
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
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    // ── shared fixtures (used by several theme files below) ──────────────

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

    mod account_scope_tests;
    mod book_batch_tests;
    mod dispatch_settlement_tests;
    mod gate_exemptions_tests;
    mod round_rollover_tests;
    mod sizing_tests;
}
