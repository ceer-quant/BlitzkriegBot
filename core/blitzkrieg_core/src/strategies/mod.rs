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
//! There is exactly ONE full-featured contract: [`EngineStrategy`]. The three
//! in-tree builtins — [`spread_arb::SpreadArbBuiltin`] (the dip buyer),
//! [`trend_follow::TrendFollowBuiltin`] (the chase leg, E4-a / #30) and
//! [`mean_reversion::MeanReversionBuiltin`] (the fade leg, E4-b / #31) — and an
//! external dylib loaded through C ABI v2 ([`foreign::ForeignStrategy`]) all
//! implement it. Being external is
//! only a loading difference — an external strategy sees every book callback,
//! the full depth ladder, round/market context, and can express entries, exits,
//! breaks, confirmation, diagnostics, config and hot parameters. The old
//! best-only reduced trait/adapter (which dropped `Sell` and no-op'd `on_book`)
//! was removed in E7 (#38) so the capability gap cannot reopen.

#[cfg(feature = "strategy-loading")]
pub mod foreign;
pub mod mean_reversion;
pub mod shadow_twin;
pub mod spread_arb;
pub mod trend_follow;

pub use mean_reversion::MeanReversionConfig;
pub use shadow_twin::{EngineStrategyShadow, ShadowFactory, ShadowTickCtx, ShadowTickResult};
pub use trend_follow::TrendFollowConfig;

use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
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
        }
    }

    pub fn any(&self) -> bool {
        self.timing || self.momentum
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

    /// Parse the JSON form used across the C ABI (`{"timing":..,"momentum":..}`).
    /// Unknown keys and non-boolean values are ignored, so a malformed
    /// declaration degrades to "not declared" and never to a wider exemption.
    pub fn from_json(v: &serde_json::Value) -> Self {
        let flag = |key: &str| v.get(key).and_then(|b| b.as_bool()).unwrap_or(false);
        Self {
            timing: flag("timing"),
            momentum: flag("momentum"),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "timing": self.timing, "momentum": self.momentum })
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
}

/// Read-only host market view handed to a strategy for one evaluation cycle.
pub struct StrategyCtx<'a> {
    markets: &'a [CryptoMarket],
    round_slot: i64,
    time_left_sec: i64,
    now_ms: i64,
    fresh_book: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
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
        }
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
        _registry: Option<std::sync::Arc<crate::shadow_evolution::ParamRegistry>>,
    ) {
    }

    /// The knobs this strategy declares evolvable, with their domains
    /// (E2-c / #28). Default = none, i.e. **not evolvable**; the host then
    /// builds no shadow variants for this strategy and reports it as such.
    ///
    /// A strategy that returns knobs also needs [`EngineStrategy::shadow_factory`],
    /// otherwise the declaration is inert (nothing can be replayed).
    fn evolvable_knobs(&self) -> Vec<crate::shadow_evolution::KnobSpec> {
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

    /// The spread_arb parameters currently in force (observability for the
    /// builtin; other strategies return None).
    fn spread_arb_view(&self) -> Option<SpreadArbConfig> {
        None
    }

    /// The config currently in force, as a JSON string (observability). Any
    /// strategy — in-tree or external — may report one; a foreign library does
    /// so through its OPTIONAL `bk_strategy_config_view` symbol, an in-tree
    /// strategy by overriding this. `None` = "nothing declared", never an error.
    fn config_view_json(&self) -> Option<String> {
        None
    }

    /// The host config changed (trend / spread_arb knobs).
    fn on_config(&mut self, _trend: &TrendConfig, _spread_arb: &SpreadArbConfig) {}
}
