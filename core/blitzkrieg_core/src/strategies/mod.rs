//! Strategy host seam — how the self-driving engine runs strategies.
//!
//! The engine owns the market state (books, spot, round timing) and the shared
//! entry gates (one-entry-per-token, round timing, spot momentum, sizing). A
//! hosted strategy only turns that state into *candidates*: it may never place,
//! size or risk-check anything itself — the host does, so every strategy is
//! subject to the same kernel-side gates. The two entry-quality gates (round
//! timing window, spot momentum) are the ONLY ones a strategy may declare an
//! exemption from ([`GateExemptions`], E2-b / #27); the exemption is explicit,
//! logged, counted and can never reach the safety boundary.
//!
//! There is exactly ONE full-featured contract: [`EngineStrategy`]. The kernel
//! ships ZERO implementations of it (PR-B hard switch): every strategy — the
//! shipped example cdylibs included — arrives through the C ABI v2 dlopen path
//! ([`foreign::ForeignStrategy`]). Being external is
//! only a loading difference — an external strategy sees every book callback,
//! the full depth ladder, round/market context, and can express entries, exits,
//! breaks, confirmation, diagnostics, config and hot parameters. The old
//! best-only reduced trait/adapter (which dropped `Sell` and no-op'd `on_book`)
//! was removed in E7 (#38) so the capability gap cannot reopen, and the in-tree
//! builtin wrappers were deleted in PR-B so there is no privileged code path: an
//! external strategy really is first-class, and nothing about strategy identity
//! is hardcoded in the kernel.

#[cfg(feature = "strategy-loading")]
pub mod foreign;
pub mod shadow_twin;
// Test-only hosted adapters (enabled for tests or when dev-support is needed).
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use shadow_twin::{EngineStrategyShadow, ShadowFactory, ShadowTickCtx, ShadowTickResult};

use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::signal::TradeSignal;
use rust_decimal::Decimal;
use std::collections::HashSet;

/// A strategy's declaration that it does NOT want one or more of the shared
/// ENTRY gates applied to its own candidates (E2-b / #27).
///
/// The default is all-false: a strategy that says nothing is gated exactly as
/// before, so adding this seam cannot change any existing behaviour. The gates
/// are entry *quality heuristics*, not safety boundaries — the risk gate, the
/// kill switch, the daily-loss cap, the sizing ceiling and the position quotas
/// are enforced downstream in `Core` and are never reachable from here.
///
/// Why it is a declaration on the strategy: a mean-reversion / reverse strategy
/// wants to enter exactly when the momentum filter rejects (spot moving against
/// the bet), and another strategy family wants a different round window. The
/// host records every honoured exemption as an auditable record («本单因策略 X
/// 豁免门禁 Y») so an opt-out is always visible in logs and in `engine.stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GateExemptions {
    /// Waive the round-timing window (`min_round_age_sec` / `min_time_left_sec`).
    /// Never waives "no market for this round" — see `TimingBlock::exemptible`.
    pub timing: bool,
    /// Waive the spot momentum alignment filter.
    pub momentum: bool,
    /// OPTIONAL lower bound (seconds) on `time_left_sec` for the `timing`
    /// exemption (D-31). `None` = use the kernel default
    /// (`scanner.min_time_left_sec`), which is the "I still respect the global
    /// time-left window" reading.
    ///
    /// Why a bound exists at all: without one, a `timing`-exempt strategy may
    /// enter inside the force-exit window, where the exit policy fires on the
    /// very next tick by design. The entry is *legal* but the round has no life
    /// left to reach a target, so the trade is a guaranteed zero-hold exit — it
    /// drags the win rate down without being a strategy misjudgement, i.e. it
    /// puts noise rather than information into the dry ledger.
    ///
    /// Semantics, stated exactly: while `timing` is exempt, this value REPLACES
    /// the scanner's `min_time_left_sec` for that strategy's candidates. So it
    /// lets the exemption keep waiving "round too young" (where `time_left_sec`
    /// is LARGE and a lower bound never conflicts) without also waiving "too
    /// close to expiry".
    ///
    /// How far it can move the gate, honestly: before D-31 `timing: true` waived
    /// the time-left gate entirely, so anything this field does is a NARROWING of
    /// that exemption — it can never widen behaviour past what already shipped.
    /// (An explicitly declared bound may of course sit below the scanner's global
    /// value; that is the per-strategy declaration doing its job, and every
    /// honoured exemption is audited downstream.) A negative value is clamped to
    /// zero, the one direction that must never be honoured.
    pub timing_min_time_left_sec: Option<i64>,
}

impl GateExemptions {
    /// Nothing waived — the unchanged, fully-gated behaviour.
    pub fn none() -> Self {
        Self::default()
    }

    /// Waive both entry gates.
    pub fn all() -> Self {
        Self {
            timing: true,
            momentum: true,
            timing_min_time_left_sec: None,
        }
    }

    pub fn any(&self) -> bool {
        self.timing || self.momentum
    }

    /// The `time_left_sec` floor in force for this strategy: its own declared
    /// value, else the kernel's `default_min` (the scanner's
    /// `min_time_left_sec`). A declared value is clamped to `>= 0`: a negative
    /// floor would restore the unbounded pre-D-31 waiver, the one direction this
    /// field must never move.
    pub fn timing_floor_sec(&self, default_min: i64) -> i64 {
        match self.timing_min_time_left_sec {
            Some(n) => n.max(0),
            None => default_min.max(0),
        }
    }

    /// Gate names in a stable order, for logs / diagnostics.
    pub fn gates(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.timing {
            out.push("timing");
        }
        if self.momentum {
            out.push("momentum");
        }
        out
    }

    /// Parse the JSON form used across the C ABI
    /// (`{"timing":..,"momentum":..,"timing_min_time_left_sec":..}`).
    /// Unknown keys and non-boolean values are ignored, so a malformed
    /// declaration degrades to "not declared" and never to a wider exemption.
    ///
    /// A library that predates D-31 returns only the two booleans: the missing
    /// key yields `None`, which means "use the kernel default" — the stricter
    /// reading, not the looser one. That is why this field could be added to the
    /// existing optional symbol without a `BK_ABI_VERSION` bump.
    pub fn from_json(v: &serde_json::Value) -> Self {
        let flag = |key: &str| v.get(key).and_then(|b| b.as_bool()).unwrap_or(false);
        Self {
            timing: flag("timing"),
            momentum: flag("momentum"),
            timing_min_time_left_sec: v.get("timing_min_time_left_sec").and_then(|n| n.as_i64()),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({ "timing": self.timing, "momentum": self.momentum });
        if let Some(n) = self.timing_min_time_left_sec {
            v["timing_min_time_left_sec"] = serde_json::json!(n);
        }
        v
    }
}

/// A strategy's wish to CLOSE an open position. Like an entry candidate it is
/// only an intent: the host resolves it against a live position, prices it off
/// the current book, runs the same risk/dedup/sizing path and submits the sell.
/// `reason` is a short strategy-owned tag for tracing (e.g. "tp"); the kernel
/// records the close under [`crate::model::ExitReason::StrategySignal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyExitIntent {
    pub token_id: String,
    pub reason: String,
    /// The account whose row the close resolves to (the same second slice key
    /// [`HeldPosition::account_id`] carries). The engine stamps it from the
    /// emitting scope, so two accounts holding the SAME token close their own
    /// rows — an unaccounted close would resolve to whichever account's
    /// position a token-only lookup found first. Serde-free (an in-process
    /// intent): the field is written by the engine at emission, never parsed,
    /// so there is no back-compat shape to keep.
    pub account_id: String,
}

/// A position slice the host hands a strategy for one evaluation cycle —
/// THIS scope's open positions only (the engine filters by strategy name AND
/// account before constructing the view). Enough to see a partially-filled
/// leg's shortfall against its pair attempt (the gap-fill loop), not enough
/// to touch another scope's book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldPosition {
    /// Owning strategy — `evaluate` slices the host's flat view by this, so a
    /// package's `bk.holdings()` never shows another package's book.
    pub strategy: String,
    /// Owning account — the SECOND slice key. The same strategy on two
    /// accounts is two independent books; without this the slice would leak
    /// account B's holdings into account A's gap-fill view. Serde default
    /// keeps pre-fanout persisted views loading as default-scope.
    pub account_id: String,
    pub condition_id: String,
    pub token_id: String,
    /// "up" | "down" — the leg label the wire uses everywhere else.
    pub direction: String,
    /// Shares currently held, as a decimal string (the wire rule).
    pub shares: String,
    pub entry_price: String,
    /// The round slot this position belongs to (a 15-minute wheel round).
    pub round_slot: i64,
}

/// Read-only host market view handed to a strategy for one evaluation cycle.
pub struct StrategyCtx<'a> {
    markets: &'a [CryptoMarket],
    round_slot: i64,
    time_left_sec: i64,
    now_ms: i64,
    fresh_book: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
    /// THIS strategy's open positions, as of the start of this cycle. Empty
    /// unless the host pushes one (`Engine::set_positions_view`) — every
    /// existing call site keeps the old shape, and strategies that never read
    /// the view cost nothing.
    held_positions: &'a [HeldPosition],
    /// #427: the confirmed cross-venue mappings (`UnifiedEvent`s), as of this
    /// cycle. Empty unless the host pushes one — every existing call site and
    /// every single-venue package keeps today's shape.
    unified_events: &'a [blitzkrieg_market_api::unified::UnifiedEvent],
    /// RFC 568 阶段一：THIS strategy's pair-book views (the `bk.arb.pairs()`
    /// 数据源)，as of this cycle. Empty unless the host pushes one — every
    /// existing call site keeps the old shape.
    pair_views: &'a [crate::pair::PairView],
}

impl<'a> StrategyCtx<'a> {
    pub fn new(
        markets: &'a [CryptoMarket],
        round_slot: i64,
        time_left_sec: i64,
        now_ms: i64,
        fresh_book: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
    ) -> Self {
        Self {
            markets,
            round_slot,
            time_left_sec,
            now_ms,
            fresh_book,
            held_positions: &[],
            unified_events: &[],
            pair_views: &[],
        }
    }

    /// The full constructor: same view plus THIS strategy's open positions
    /// (the gap-fill surface). The engine's real evaluation cycle is the only
    /// production caller.
    pub fn with_positions(mut self, held: &'a [HeldPosition]) -> Self {
        self.held_positions = held;
        self
    }

    /// #427: attach the confirmed cross-venue mappings for this cycle. The
    /// engine's real evaluation cycle is the only production caller.
    pub fn with_unified_events(
        mut self,
        events: &'a [blitzkrieg_market_api::unified::UnifiedEvent],
    ) -> Self {
        self.unified_events = events;
        self
    }

    /// RFC 568 阶段一：attach THIS strategy's pair-book views for this cycle
    /// （owner 过滤已在 Core 侧做完 —— 跨策略/跨账户不可见）。The engine's
    /// real evaluation cycle is the only production caller.
    pub fn with_pair_views(mut self, views: &'a [crate::pair::PairView]) -> Self {
        self.pair_views = views;
        self
    }

    /// Markets of the current round.
    pub fn markets(&self) -> &[CryptoMarket] {
        self.markets
    }
    pub fn round_slot(&self) -> i64 {
        self.round_slot
    }
    pub fn time_left_sec(&self) -> i64 {
        self.time_left_sec
    }
    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }
    /// THIS strategy's open positions for this cycle. Empty when the host did
    /// not push a view (diagnostics paths, shadow twins).
    pub fn held_positions(&self) -> &[HeldPosition] {
        self.held_positions
    }
    /// #427: the confirmed cross-venue mappings for this cycle. Empty when
    /// the host did not push one (diagnostics paths, shadow twins) — a
    /// cross-venue package treats that as "no paired event", never as an
    /// error.
    pub fn unified_events(&self) -> &[blitzkrieg_market_api::unified::UnifiedEvent] {
        self.unified_events
    }
    /// RFC 568 阶段一：THIS strategy's pair-book views for this cycle. Empty
    /// when the host did not push one（诊断路径、影子孪生）——策略读不到
    /// 配对簿时按「无登记」处理，绝不视为错误。
    pub fn pair_views(&self) -> &[crate::pair::PairView] {
        self.pair_views
    }
    /// Token book, but only while it is fresh enough to price off.
    pub fn fresh_book(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        (self.fresh_book)(token_id)
    }
}

/// A strategy hosted by the engine.
///
/// Lifecycle: `on_config` (on engine config changes) → `on_round` + `on_book`
/// as data arrives → `find_candidates` once per evaluation. The host then runs
/// the shared gates and places the surviving orders.
pub trait EngineStrategy: Send + Sync {
    fn name(&self) -> &str;

    /// Observe a book / top-of-book update (own trend/price state).
    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64);

    /// A new round started (per-round state reset). Carries the round's real
    /// timing — the slot, seconds remaining, and the host clock at the
    /// transition — so a strategy never has to infer a round change from a
    /// book tick or run on a zeroed clock.
    fn on_round(&mut self, slot: i64, time_left_sec: i64, now_ms: i64);

    /// E29 (§10.4): a K-line bar CLOSED (`is_closed = true` — only closed
    /// bars are ever pushed, so a strategy never filters). Receives the SAME
    /// `Kline` type the strategy API re-exports, so a cdylib sees one shape.
    /// Default no-op: strategies that ignore K-lines are untouched by the
    /// dispatch (vtable grows but every existing behaviour is identical).
    fn on_kline(&mut self, _kline: &crate::kline::Kline) {}

    /// Tokens whose setup broke since the last call; the host cancels their
    /// resting entry bids. Drains: each break is returned exactly once.
    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        Vec::new()
    }

    /// Tokens this strategy currently considers entry-eligible. Feeds the shadow
    /// observation and diagnostics; the host unions it across strategies.
    fn confirmed_tokens(&self) -> HashSet<String> {
        HashSet::new()
    }

    /// Entry candidates for this cycle. The host applies the shared gates
    /// (round timing, spot momentum, one-entry-per-token) and sizing afterwards,
    /// so strategies cannot bypass them — except for the entry-quality gates
    /// this strategy explicitly declares an exemption for via
    /// [`EngineStrategy::gate_exemptions`], which the host honours and records.
    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal>;

    /// The shared ENTRY gates this strategy declares it does not need applied to
    /// its own candidates (E2-b / #27). Default = none, i.e. fully gated.
    ///
    /// Read by the host, cached per strategy and recorded; it cannot waive the
    /// structural precondition ("no market this round") nor anything in the
    /// safety boundary (risk gate, kill switch, daily-loss cap, sizing ceiling,
    /// position quotas), all of which are enforced downstream in `Core`.
    fn gate_exemptions(&self) -> GateExemptions {
        GateExemptions::none()
    }

    /// Whether this strategy's positions are meant to be HELD TO SETTLEMENT
    /// (expiry redemption) rather than sold on the exit ladder.
    ///
    /// Why it exists: a complete-set pair (UP + DOWN bought below $1) pays
    /// exactly $1 per share-pair at settlement regardless of which side wins.
    /// Selling either leg before expiry converts a riskless payoff into a
    /// directional trade, so the exit policy's TimeExit ladder must leave these
    /// positions alone; the host's venue settlement path (issue #175) then
    /// closes them at their redemption value.
    ///
    /// NOT a safety bypass: sizing, risk gate, kill switch and quotas still
    /// apply to every entry, and an explicit strategy exit intent is always
    /// honoured. Default: false (every existing strategy keeps its exits).
    fn holds_to_settlement(&self) -> bool {
        false
    }

    /// The strategy's own verdict on the trailing stop, as a THREE-STATE
    /// declaration: `None` = follow the kernel-wide value (`risk.setExit`
    /// trailingEnabled, the operator's lever); `Some(true)` = this strategy
    /// wants profit-banking sells even when the kernel value is off;
    /// `Some(false)` = this strategy must never see one even when the kernel
    /// value is on.
    ///
    /// Why the declaration exists: issue 356's calibration (trailing OFF
    /// shipped) came from ONE operator's history — every exit was a REDEEM/
    /// MERGE, so trailing banking was a counter-factual behaviour for them.
    /// spread_arb's mean-reversion exits are the opposite shape: its +46.7%
    /// peak dissolved into a stop-loss because nothing banked the profit.
    /// A kernel-wide flag forces those two philosophies onto one switch and
    /// re-opens the same wound whichever way it lands; the strategy itself
    /// knows which exit doctrine its positions follow.
    ///
    /// Same authority shape as [`EngineStrategy::holds_to_settlement`]: NOT a
    /// safety bypass — the protective stop, take-profit, time exits, force
    /// exit and every quota still apply; this only arms/disarms the ONE
    /// profit-trail rule. Default `None` (every existing strategy follows the
    /// kernel value byte-identically).
    fn wants_trailing_exit(&self) -> Option<bool> {
        None
    }

    /// E-VA: whether this strategy's entry candidates form an ATOMIC BATCH —
    /// the host must reserve the WHOLE batch's virtual quota (the
    /// `virtual_limits` pot) BEFORE the first leg is placed, and a pot that
    /// cannot cover every leg cancels the batch with zero legs out (a pair
    /// with one naked leg is a different, worse trade than no trade).
    ///
    /// Read by the submission loops in `Core` before the per-order gates; it
    /// changes nothing unless the strategy ALSO sits in
    /// `CoreConfig::virtual_limits` (the pot arming). Default: false — every
    /// existing strategy keeps the per-order path, and a single-leg batch
    /// from an undeclared strategy is byte-identical to today.
    fn atomic_batch(&self) -> bool {
        false
    }

    /// #642: the strategy's pair-declaration DEFAULT — the `{key, leg}` label
    /// pair attached to every entry whose own declaration is absent. The Lua
    /// side carries it per-entry (`entries` 行的可选 `pair` 表，经
    /// lua_loader 透传为 `TradeSignal::pair`)；cdylib 侧经
    /// `bk_strategy_entry_pair`（缺省 default）+ evaluate JSON 的 per-entry
    /// `pair` 字段两通道表达，字段语义逐字对齐：`key` 是策略自报的可读标
    /// 签（防重不依赖它 —— 内核按 (owner 实例, asset, slot) 经济身份判定），
    /// `leg` 是腿标签（仅可读，方向不入经济键）。Default: `None` — every
    /// existing strategy keeps the unpaired (bit-identical) path.
    fn entry_pair(&self) -> Option<crate::signal::TradePairDecl> {
        None
    }

    /// Entry fill-policy doctrine: every entry this strategy emits is a PURE
    /// MAKER quote-pull — the host builds its `OrderRequest`s as
    /// `FillPolicy::Maker` (post-only, join the ask) and submits them with the
    /// OME's never-escalate clock (`maker_timeout_ms < 0`), so an unfilled
    /// maker leg simply rests to the round's own end-of-life; the taker chase
    /// a `MakerThenTaker` entry arms never exists. The doctrine is
    /// package-level (a pair collector that pays taker fees on either leg is a
    /// different, worse trade), so it declares once and every entry inherits
    /// it — the same authority shape as [`EngineStrategy::wants_trailing_exit`]:
    /// NOT a safety bypass (risk gates, sizing, quotas and the batch pre-check
    /// all still apply; only WHERE the order rests changes). Default false —
    /// every existing strategy keeps today's `MakerThenTaker` +
    /// `entry_maker_timeout_ms` behaviour byte-identically.
    fn entry_maker(&self) -> bool {
        false
    }

    /// Close intents accumulated since the last drain. The host resolves each
    /// token to a live position and routes it through the SAME exit submission
    /// path as an automated/policy exit (live-sell dedup, `sell_shares`, risk +
    /// ledger + sign in `place`); an intent on a token with no open position is
    /// dropped. Drains: each intent is returned at most once.
    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
        Vec::new()
    }

    /// Optional diagnostics payload (surfaced per strategy by `engine.stats`).
    fn diagnostics(&self, _ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        Vec::new()
    }

    /// Shadow-Evolution hot parameters (E2-c / #28).
    ///
    /// The host hands over the **per-strategy registry**; a strategy that
    /// declares evolvable knobs resolves its OWN cell
    /// (`registry.handle_for(self.name())`) and reads it lock-free on the hot
    /// path. A strategy that declares nothing simply ignores the call — which is
    /// an explicit "not evolvable", not an error.
    ///
    /// `None` DETACHES the overlay, which is what the host passes while Shadow
    /// Evolution is disabled: with no cell the live config stands alone, so a
    /// core that never runs evolution behaves exactly as it did before the
    /// feature existed (acceptance: "evolution off ⇒ unchanged"). Default: ignore.
    fn set_hot_params(
        &mut self,
        _registry: Option<std::sync::Arc<crate::registry::ParamRegistry>>,
    ) {
    }

    /// The knobs this strategy declares evolvable, with their domains
    /// (E2-c / #28). Default = none, i.e. **not evolvable**; the host then
    /// builds no shadow variants for this strategy and reports it as such.
    ///
    /// A strategy that returns knobs also needs [`EngineStrategy::shadow_factory`],
    /// otherwise the declaration is inert (nothing can be replayed).
    fn evolvable_knobs(&self) -> Vec<strategy_logic::params::KnobSpec> {
        Vec::new()
    }

    /// How to build a twin of this strategy for the shadow comparison
    /// (E2-c / #28). The twin is an INDEPENDENT instance running this
    /// strategy's own logic with a parameter set applied, so a variant is
    /// evaluated by the strategy itself rather than by a kernel-side copy.
    /// Default: none (not evolvable).
    fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
        None
    }

    /// The config currently in force, as a JSON string (observability). Any
    /// strategy may report one; a foreign library does so through its OPTIONAL
    /// `bk_strategy_config_view` symbol. `None` = "nothing declared", never an
    /// error. This is the ONLY config-observability surface — the kernel keeps
    /// no knowledge of any strategy's parameter names.
    fn config_view_json(&self) -> Option<String> {
        None
    }

    /// RFC-645 (A-1, 决议③): whether this strategy CONSUMES the pair-book
    /// statistics surface — the `arb*` fields (`arbNetPnlUsd` / `arbWins` /
    /// `arbLosses` / `arbClosedTrades` / `arbActivePairs`) the host attaches to
    /// the strategy's `strategy.stats` row.
    ///
    /// Why a declaration: the statistic fields' content is computed from the
    /// strategy's OWN accounting keys and the PairBook's `owner_strategy`
    /// filter, so the kernel needs no strategy knowledge to serve them — only
    /// the answer to "does this strategy want the pair surface?". The
    /// pre-RFC-645 arbiter was a NAME comparison against the one privileged
    /// builtin (`cross_venue_arb`); this declaration is what replaces the
    /// name (the name branch survives one release cycle as a deprecated
    /// alias, then M3 deletes it).
    ///
    /// NOT unconditional: the host ACCEPTS the declaration only when the
    /// strategy also declares `atomic_batch()` (a pair consumer that does not
    /// run atomic batches cannot keep its legs whole) AND the PairBook holds
    /// at least one live entry for it (it actually produces pairs). A claim
    /// that fails the check is treated as undeclared, with a warn — the
    /// fail-closed shape resolution ③ pinned.
    ///
    /// Default: `false` — every existing strategy keeps its row untouched.
    fn consumes_pair_book_stats(&self) -> bool {
        false
    }

    /// The host config changed — the ONE opaque-JSON push (RFC-645 A-2):
    /// the kernel serialises each strategy's OWN config projection to JSON at
    /// the dispatch site and hands it over here; a strategy deserialises the
    /// fields it knows back into its own types (a parse failure is the
    /// config-rejection path, advisory — never a strategy crash). The JSON's
    /// camelCase spelling is the dylib wire contract pinned byte-for-byte by
    /// the golden test (`engine_strategy_config_json_is_the_foreign_marshal_golden`
    /// in `strategies::test_support`); the trait used to carry the two builtin
    /// config types directly, which was the one type-level leak RFC-645 A-2
    /// retires — the C ABI (`strategy_api/src/lib.rs` `on_config`) has been
    /// opaque JSON all along, and the trait now faces the same shape.
    ///
    /// Default: no-op — a strategy that reads nothing of the push (the Lua
    /// adapter's packages, any strategy without config knobs) keeps today's
    /// behaviour exactly.
    fn on_config(&mut self, _config: &serde_json::Value) {}

    /// E30 (§6.3): the one-shot poison alert from a sandboxed runtime (the
    /// Lua state machine breached its instruction budget or memory ceiling).
    /// The sandbox refuses every later call itself; this drains the ONE
    /// alert message the host raises as a single RISK_ALERT naming the
    /// strategy. Default: none — every non-sandboxed strategy answers `None`
    /// forever.
    fn poison_alert(&mut self) -> Option<String> {
        None
    }

    /// 跨所套利弹窗 v2: the arb leg configuration in force (the accounts.toml
    /// `[cross_venue_arb]` section, memory-mirrored on the engine). Two
    /// consumers ride one push: the adapter's account STAMP (entry signals
    /// and exit intents carry the token's OWN venue's leg account, falling
    /// back to the instance account for venues outside the configuration)
    /// and the strategy's `bk.params()` bag (the leg venues/accounts/pairs
    /// the pairing logic reads). `None` = unconfigured: the stamp falls back
    /// everywhere (instance account — the pre-v2 behaviour) and no params
    /// entries are added. Default: ignore — every strategy that never opted
    /// into the cross-venue flow keeps today's behaviour.
    fn set_arb_leg_config(&mut self, _cfg: Option<crate::account::CrossVenueArbConfig>) {}

    /// RFC 568 阶段一：THIS instance's own account scope (the instance's
    /// registration account) —— 配对声明落账时经济键第二元（owner 实例隔离
    /// 维度）的权威来源：腿的 `account_id` 是 stamp 结果（v2 可 A≠B），不
    /// 能混作实例身份。Default = the default account（与注册缺省同形）。
    fn instance_account(&self) -> String {
        blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string()
    }

    /// 跨所套利弹窗 v2: the leg accounts this strategy's OWN book may span —
    /// the configured pair's two account ids when the strategy runs under a
    /// leg configuration, `None` otherwise. The engine widens the scope's
    /// `bk.holdings()` view with rows under THESE accounts only (the stamp
    /// sends each leg to its own venue's account, so the release decision —
    /// which prices the WHOLE pair — must see both legs' held rows). Default:
    /// `None` — every non-cross-venue strategy keeps the strictly per-scope
    /// slicing, byte-identical.
    fn arb_leg_accounts(&self) -> Option<Vec<String>> {
        None
    }
}

#[cfg(test)]
mod held_position_tests {
    use super::*;

    /// The ctx's position view is exactly what the host pushed — no strategy
    /// slicing, no re-derivation. Slicing happens in the ENGINE (engine.rs
    /// filters by strategy name before calling `find_candidates`), so the
    /// builder contract is: `new()` = empty, `with_positions` = the slice.
    #[test]
    fn ctx_positions_default_empty_and_builder_installs_the_slice() {
        let markets: Vec<crate::model::CryptoMarket> = Vec::new();
        let fresh = |_: &str| None;
        let ctx = StrategyCtx::new(&markets, 1, 60, 1_000, &fresh);
        assert!(ctx.held_positions().is_empty(), "default view is empty");

        let held = vec![HeldPosition {
            account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
            strategy: "pair_discount_arb".into(),
            condition_id: "cond".into(),
            token_id: "tok".into(),
            direction: "up".into(),
            shares: "3.00".into(),
            entry_price: "0.42".into(),
            round_slot: 7,
        }];
        let ctx = StrategyCtx::new(&markets, 1, 60, 1_000, &fresh).with_positions(&held);
        assert_eq!(ctx.held_positions().len(), 1);
        assert_eq!(ctx.held_positions()[0].token_id, "tok");
        assert_eq!(ctx.held_positions()[0].shares, "3.00");
    }
}
