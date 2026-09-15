//! Shadow twins: replay a strategy's **own** entry/exit logic against a
//! counterfactual parameter set (E2-c / #28).
//!
//! The pre-E2-c shadow variant re-implemented spread_arb's entry discipline
//! inside the kernel, so "evaluate the variant" meant "evaluate a kernel-side
//! copy of one strategy". That cannot be extended to a second strategy, and it
//! drifts from the strategy it claims to model.
//!
//! Now a strategy produces a **twin**: an independent instance of itself with a
//! parameter set applied. The kernel wraps that twin in
//! [`EngineStrategyShadow`], which builds the same [`StrategyCtx`] a live
//! evaluation gets and calls the twin's own `find_candidates` /
//! `take_exit_intents`. Whatever the strategy decides for real is what the
//! shadow decides — in-tree or loaded from a dylib (external remains only a
//! loading difference).

use super::{EngineStrategy, StrategyCtx};
use crate::exit_policy::{decide_exit, executable_bid, taker_fee_pct, update_exit_state, ExitConfig, ExitState, ExitTickInput};
use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::shadow_evolution::{KnobSpec, StrategyParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet, VecDeque};

/// One strategy's shadow contract: what it declares evolvable, and how to build
/// an independent twin with a parameter set applied.
///
/// A strategy that returns `None` from `EngineStrategy::shadow_factory` — or an
/// empty knob declaration — has explicitly declared itself **not evolvable**.
/// The kernel then builds no variants for it and records that fact, rather than
/// inventing a counterfactual it cannot honestly run.
pub trait ShadowFactory: Send + Sync {
    fn strategy(&self) -> String;
    /// Declared knobs: names, the values in force, and the domains.
    fn knobs(&self) -> Vec<KnobSpec>;
    /// Build a twin with `params` applied to its own logic. `None` when the twin
    /// cannot be created (e.g. the source strategy is gone).
    fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>>;
}

/// The instantaneous, read-only view a shadow twin gets for one tick: the same
/// shape a live evaluation sees (round markets, this token's book, round
/// timing). No credentials, OME or venue handle is reachable from here, so a
/// twin can only ever produce an intent.
pub struct ShadowTickCtx<'a> {
    pub markets: &'a [CryptoMarket],
    pub token_id: &'a str,
    pub book: &'a OrderbookSnapshot,
    pub round_slot: i64,
    pub time_left_sec: i64,
    pub now_ms: i64,
}

/// What one shadow tick produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShadowTickResult {
    /// The twin would rest this entry bid (price only; the kernel sizes it).
    pub entry: Option<Decimal>,
    /// The twin's own close reasons for this token, in order.
    pub exit_reasons: Vec<String>,
}

/// Wraps a strategy twin so the shadow engine can replay a tick through the
/// strategy's own code path.
pub struct EngineStrategyShadow {
    inner: Box<dyn EngineStrategy>,
}

impl EngineStrategyShadow {
    pub fn new(inner: Box<dyn EngineStrategy>) -> Self {
        Self { inner }
    }

    pub fn name(&self) -> String {
        self.inner.name().to_string()
    }

    /// A new round: reset the twin's per-round state exactly where the live
    /// instance resets its own, then seed it with the same mids the live engine
    /// seeds (a round start resets the live trend tracker and immediately
    /// re-feeds it the current book, so the twin must do the same or its
    /// confirmation clock would start a tick late).
    pub fn on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, OrderbookSnapshot)],
        now_ms: i64,
    ) {
        let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
        self.inner.on_round(slot);
        for (token_id, snap) in seeds {
            self.inner.on_book(token_id, snap, now_ms);
        }
    }

    /// Feed the tick and ask the twin what it would do.
    ///
    /// Only the ticked token is served as a fresh book: a shadow tick models
    /// "the same instant the live engine saw", not a full-book replay. A
    /// strategy that needs the opposite side's book simply finds none there,
    /// exactly as it would when the live host has no fresh book for it.
    pub fn on_tick(&mut self, ctx: &ShadowTickCtx<'_>) -> ShadowTickResult {
        self.inner.on_book(ctx.token_id, ctx.book, ctx.now_ms);

        let token = ctx.token_id;
        let book = ctx.book;
        let fresh = move |t: &str| -> Option<OrderbookSnapshot> {
            if t == token {
                Some(book.clone())
            } else {
                None
            }
        };
        let sctx = StrategyCtx::new(ctx.markets, ctx.round_slot, ctx.time_left_sec, ctx.now_ms, &fresh);
        let mut out = ShadowTickResult::default();
        for sig in self.inner.find_candidates(&sctx) {
            if sig.token_id == token && out.entry.is_none() {
                out.entry = Some(sig.price);
            }
        }
        // The twin's own close logic, where it has any. A strategy without exit
        // intents is exited by the shared exit policy (D-2), which the caller
        // replays — so both mechanisms are covered without a special case.
        for intent in self.inner.take_exit_intents() {
            if intent.token_id == token {
                out.exit_reasons.push(intent.reason);
            }
        }
        // Regime-break cancels are drained and deliberately IGNORED: they are a
        // strategy-level risk control that no declared knob drives, so folding
        // them into the counterfactual would push every variant in the same
        // direction and wash out the comparison (SHADOW_EVOLUTION.md "保真度").
        let _ = self.inner.take_breaks();
        out
    }
}

/// A virtual position opened by a twin, priced by the shared exit policy.
#[derive(Debug, Clone)]
struct TwinPosition {
    entry_price: Decimal,
    shares: Decimal,
    expires_at_ms: i64,
    state: ExitState,
}

/// One virtual variant: a strategy twin driven tick-by-tick with the shared exit
/// policy managing its virtual positions. Observation-only — it holds no
/// ledger, OME or venue handle, so it cannot reach real money.
pub struct TwinReplay {
    twin: EngineStrategyShadow,
    exit_cfg: ExitConfig,
    open: HashMap<String, TwinPosition>,
    /// (exit_ms, net_pnl) of closed virtual trades, oldest first.
    trades: VecDeque<(i64, Decimal)>,
}

impl TwinReplay {
    pub fn new(twin: Box<dyn EngineStrategy>, exit_cfg: &ExitConfig) -> Self {
        Self {
            twin: EngineStrategyShadow::new(twin),
            exit_cfg: exit_cfg.clone(),
            open: HashMap::new(),
            trades: VecDeque::new(),
        }
    }

    pub fn strategy(&self) -> String {
        self.twin.name()
    }

    pub fn open_positions(&self) -> usize {
        self.open.len()
    }

    /// A new round: reset the twin's state, re-seed it with the round's current
    /// mids (exactly what the live engine does), and drop virtual positions
    /// whose market has left the round (live would have force-exited them).
    /// Closed trades survive so metrics accumulate across round boundaries (D-3).
    pub fn on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.twin.on_round(markets, seeds, now_ms);
        let valid: HashSet<&str> = markets
            .iter()
            .flat_map(|m| [m.up_token_id.as_str(), m.down_token_id.as_str()])
            .collect();
        self.open.retain(|t, _| valid.contains(t.as_str()));
    }

    /// Feed one tick through the twin's own logic, then manage the virtual
    /// position with the shared exit policy (D-2: the SAME policy live runs).
    pub fn on_tick(&mut self, ctx: &ShadowTickCtx<'_>) {
        let token = ctx.token_id;

        if let Some(pos) = self.open.get_mut(token) {
            update_exit_state(&mut pos.state, pos.entry_price, Some(ctx.book), ctx.now_ms, &self.exit_cfg);
            let time_left = (pos.expires_at_ms - ctx.now_ms) / 1000;
            let hold = (ctx.now_ms - (pos.expires_at_ms - 900_000)).max(0) / 1000;
            let policy_exit = decide_exit(ExitTickInput {
                entry_price: pos.entry_price,
                book: Some(ctx.book),
                fallback_price: Some(ctx.book.mid_price),
                time_left_sec: time_left,
                hold_sec: hold,
                state: &pos.state,
                now_ms: ctx.now_ms,
                cfg: &self.exit_cfg,
            })
            .is_some();
            // Ask the twin itself as well: a strategy-owned close is the
            // strategy's own decision, so it is honoured alongside the policy.
            let twin_exit = !catch(&mut self.twin, ctx).exit_reasons.is_empty();
            if policy_exit || twin_exit {
                let exit_price = executable_bid(Some(ctx.book));
                let (entry, shares) = (pos.entry_price, pos.shares);
                let gross = (exit_price - entry) * shares;
                let fee = taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
                self.trades.push_back((ctx.now_ms, gross - fee));
                self.open.remove(token);
            }
            return;
        }

        if let Some(entry) = catch(&mut self.twin, ctx).entry {
            self.open.insert(
                token.to_string(),
                TwinPosition {
                    entry_price: entry,
                    shares: dec!(10),
                    expires_at_ms: ctx.now_ms + ctx.time_left_sec.max(0) * 1000,
                    state: ExitState::new(entry, ctx.now_ms),
                },
            );
        }
    }

    /// Drop closed trades older than the window and return a snapshot of the
    /// rest as `(exit_ms, net_pnl)`, oldest first. The caller (the evolution
    /// evaluator) owns the metric definitions so they live in exactly one place.
    pub fn windowed_trades(&mut self, window_secs: i64, now_ms: i64) -> Vec<(i64, Decimal)> {
        let cutoff = now_ms - window_secs * 1000;
        while let Some((t, _)) = self.trades.front() {
            if *t < cutoff {
                self.trades.pop_front();
            } else {
                break;
            }
        }
        self.trades.iter().copied().collect()
    }

    /// Total closed virtual trades ever (not windowed) — observability only.
    pub fn closed_trades(&self) -> usize {
        self.trades.len()
    }
}

/// Run one tick through the twin under `catch_unwind`. A panicking twin returns
/// an empty result; the caller quarantines it on the panic, and nothing ever
/// propagates into the live strategy path.
pub fn catch(twin: &mut EngineStrategyShadow, ctx: &ShadowTickCtx<'_>) -> ShadowTickResult {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| twin.on_tick(ctx))).unwrap_or_default()
}

/// Assemble the read-only tick view a twin is served (no credentials, no
/// venue, no OME — structurally out of reach).
pub fn tick_ctx<'a>(
    markets: &'a [CryptoMarket],
    token_id: &'a str,
    book: &'a OrderbookSnapshot,
    round_slot: i64,
    time_left_sec: i64,
    now_ms: i64,
) -> ShadowTickCtx<'a> {
    ShadowTickCtx { markets, token_id, book, round_slot, time_left_sec, now_ms }
}
