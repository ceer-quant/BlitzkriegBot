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
use crate::exit_policy::{
    ExitConfig, ExitState, ExitTickInput, decide_exit, taker_fee_pct, update_exit_state,
};
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
        // The twin's caller knows only the host clock at the replay tick; the
        // engine's `RoundMarkets` transition carries the true seconds-left, so
        // derive the same number from the round's expiry when it is available
        // and pass 0 otherwise (mirrors a round with no known end).
        let time_left_sec = markets
            .first()
            .map(|m| ((m.expires_at_ms - now_ms) / 1000).max(0))
            .unwrap_or(0);
        self.inner.on_round(slot, time_left_sec, now_ms);
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
            if t == token { Some(book.clone()) } else { None }
        };
        let sctx = StrategyCtx::new(
            ctx.markets,
            ctx.round_slot,
            ctx.time_left_sec,
            ctx.now_ms,
            &fresh,
        );
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

/// The fixed virtual position size of a twin. The live host owns sizing; a
/// shadow can never reach it, so the replay carries this constant — but the
/// fillability checks below still require the book to actually cover it.
const TWIN_SHARES: Decimal = dec!(10);

/// A virtual position opened by a twin, priced by the shared exit policy.
#[derive(Debug, Clone)]
struct TwinPosition {
    entry_price: Decimal,
    shares: Decimal,
    expires_at_ms: i64,
    state: ExitState,
}

/// F7 — entry fillability: how much the counterparty (offer) side is actually
/// willing to trade at prices **not worse than** the entry limit. A resting
/// buy at `limit` only fills against asks priced at or below it, so an entry
/// signal whose quote no seller honours must not become a position.
fn fillable_ask_size(book: &OrderbookSnapshot, limit: Decimal) -> Decimal {
    book.asks
        .iter()
        .filter(|(p, _)| *p <= limit)
        .map(|(_, s)| *s)
        .sum()
}

/// F7 — exit fillability: the price a long of `shares` can actually SELL at
/// right now, as the volume-weighted sweep of the bid ladder. `None` when
/// there is no bid at all, or the ladder cannot cover the size: a mid or a
/// reference price is never an executable exit (the +0.421937 phantom the
/// audit caught came exactly from that fallback).
fn executable_bid_for_size(book: &OrderbookSnapshot, shares: Decimal) -> Option<Decimal> {
    if book.best_bid <= Decimal::ZERO {
        return None;
    }
    let mut remaining = shares;
    let mut notional = Decimal::ZERO;
    for (p, s) in &book.bids {
        let take = (*s).min(remaining);
        notional += *p * take;
        remaining -= take;
        if remaining <= Decimal::ZERO {
            break;
        }
    }
    if remaining > Decimal::ZERO || shares <= Decimal::ZERO {
        return None;
    }
    Some(notional / shares)
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
    /// Last KNOWN-executable bid per token (F7): the only price a position
    /// forced out at a round boundary may settle at. A token whose book never
    /// showed a deep-enough bid has none, and its settlement falls back to a
    /// full principal loss instead of an invented price.
    last_bids: HashMap<String, Decimal>,
}

impl TwinReplay {
    pub fn new(twin: Box<dyn EngineStrategy>, exit_cfg: &ExitConfig) -> Self {
        Self {
            twin: EngineStrategyShadow::new(twin),
            exit_cfg: exit_cfg.clone(),
            open: HashMap::new(),
            trades: VecDeque::new(),
            last_bids: HashMap::new(),
        }
    }

    pub fn strategy(&self) -> String {
        self.twin.name()
    }

    pub fn open_positions(&self) -> usize {
        self.open.len()
    }

    /// A new round: reset the twin's state, re-seed it with the round's current
    /// mids (exactly what the live engine does), and settle the virtual
    /// positions whose market has left the round (live would have force-exited
    /// them). Closed trades survive so metrics accumulate across round
    /// boundaries (D-3).
    ///
    /// F7: settling is mandatory — a dropped position is never silently
    /// deleted. With a last-known executable bid it books the exit at that bid
    /// (same accounting as any other exit); with none, the position's price
    /// can no longer be sold at any known level, so the FULL entry cost is
    /// booked as an unrealized-principal loss. Either way the trade stays in
    /// the statistics instead of vanishing with its loss.
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
        let dropped: Vec<(String, TwinPosition)> = self
            .open
            .iter()
            .filter(|(t, _)| !valid.contains(t.as_str()))
            .map(|(t, p)| (t.clone(), p.clone()))
            .collect();
        for (token, pos) in dropped {
            let pnl = match self.last_bids.get(&token).copied() {
                Some(bid) if bid > Decimal::ZERO => {
                    let gross = (bid - pos.entry_price) * pos.shares;
                    let fee = taker_fee_pct(bid) / Decimal::ONE_HUNDRED * bid * pos.shares;
                    gross - fee
                }
                _ => {
                    // 未结算-本金损失: no executable bid was ever seen, so the
                    // entry cost is gone in full.
                    -(pos.entry_price * pos.shares)
                }
            };
            self.trades.push_back((now_ms, pnl));
            self.last_bids.remove(&token);
        }
        self.open.retain(|t, _| valid.contains(t.as_str()));
        self.last_bids
            .retain(|t, _| valid.contains(t.as_str()) || self.open.contains_key(t));
    }

    /// Feed one tick through the twin's own logic, then manage the virtual
    /// position with the shared exit policy (D-2: the SAME policy live runs).
    /// Returns whether the TWIN'S OWN code panicked (KI-23): the panic is
    /// absorbed here — nothing propagates — but the caller sees it, so a
    /// permanently panicking twin can be quarantined instead of re-ticked
    /// forever. The exit-policy path keeps `?`-free defaulting to `false`:
    /// the shared policy is kernel code, still covered by the outer catch.
    pub fn on_tick(&mut self, ctx: &ShadowTickCtx<'_>) -> bool {
        let token = ctx.token_id;

        // F7: remember this token's last KNOWN-executable bid while it ticks,
        // so a position the round boundary takes away can settle at a real
        // price (see `on_round`) instead of an invented one.
        if let Some(bid) = executable_bid_for_size(ctx.book, TWIN_SHARES) {
            self.last_bids.insert(token.to_string(), bid);
        }

        if let Some(pos) = self.open.get_mut(token) {
            update_exit_state(
                &mut pos.state,
                pos.entry_price,
                Some(ctx.book),
                ctx.now_ms,
                &self.exit_cfg,
            );
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
            let (twin_res, twin_panicked) = catch(&mut self.twin, ctx);
            let twin_exit = !twin_res.exit_reasons.is_empty();
            if twin_panicked {
                return true;
            }
            if policy_exit || twin_exit {
                // F7: an exit must FILL — against a bid that exists and is at
                // least as deep as the position. No bid (or a thin one) means
                // the close cannot happen this tick: the position stays open
                // and is settled at a later tick or at the round boundary,
                // never priced off a mid or a reference value.
                if let Some(exit_price) = executable_bid_for_size(ctx.book, pos.shares) {
                    let (entry, shares) = (pos.entry_price, pos.shares);
                    let gross = (exit_price - entry) * shares;
                    let fee =
                        taker_fee_pct(exit_price) / Decimal::ONE_HUNDRED * exit_price * shares;
                    self.trades.push_back((ctx.now_ms, gross - fee));
                    self.open.remove(token);
                }
            }
            return false;
        }

        let (res, panicked) = catch(&mut self.twin, ctx);
        if let Some(entry) = res.entry {
            // F7: no instantaneous free fill. The twin's entry is a resting
            // bid; it only opens a position when the counterparty (offer) side
            // actually covers the full size at prices not worse than that
            // limit. Otherwise this tick brings no entry — a later tick whose
            // book does cover it may.
            if fillable_ask_size(ctx.book, entry) >= TWIN_SHARES {
                self.open.insert(
                    token.to_string(),
                    TwinPosition {
                        entry_price: entry,
                        shares: TWIN_SHARES,
                        expires_at_ms: ctx.now_ms + ctx.time_left_sec.max(0) * 1000,
                        state: ExitState::new(entry, ctx.now_ms),
                    },
                );
            }
        }
        panicked
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

/// Run one tick through the twin under `catch_unwind`. A panicking twin yields
/// `(default result, true)`: the panic is ABSORBED here — nothing ever
/// propagates into the live strategy path — but the boolean surfaces it so the
/// caller (`TwinReplay::on_tick`) can report it and the evolution loop can
/// quarantine a permanently panicking twin (KI-23) instead of re-ticking it
/// forever, silently.
pub fn catch(twin: &mut EngineStrategyShadow, ctx: &ShadowTickCtx<'_>) -> (ShadowTickResult, bool) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| twin.on_tick(ctx))) {
        Ok(res) => (res, false),
        Err(_) => (ShadowTickResult::default(), true),
    }
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
    ShadowTickCtx {
        markets,
        token_id,
        book,
        round_slot,
        time_left_sec,
        now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SignalDirection;
    use crate::signal::TradeSignal;
    use crate::strategies::StrategyExitIntent;
    use rust_decimal::prelude::FromPrimitive;

    /// A test-only twin: emits a fixed entry price (and, when asked, a close
    /// intent) on every tick, so a test can pin the exact quote and let the
    /// replay's own fillability check decide whether a position may exist.
    struct StubStrategy {
        token: String,
        entry: Option<Decimal>,
        exit: bool,
    }

    impl EngineStrategy for StubStrategy {
        fn name(&self) -> &str {
            "stub"
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64, _tl: i64, _now: i64) {}
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            let Some(price) = self.entry else {
                return Vec::new();
            };
            let market = &ctx.markets()[0];
            vec![TradeSignal {
                strategy: "stub".into(),
                asset: market.asset.clone(),
                direction: SignalDirection::Up,
                token_id: self.token.clone(),
                condition_id: market.condition_id.clone(),
                price,
                reason: "stub".into(),
            }]
        }
        fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
            if self.exit {
                vec![StrategyExitIntent {
                    token_id: self.token.clone(),
                    reason: "stub-close".into(),
                }]
            } else {
                Vec::new()
            }
        }
    }

    fn market(up: &str, down: &str) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: up.into(),
            down_token_id: down.into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 900_000,
            round_slot: 1,
            neg_risk: false,
            question: "?".into(),
        }
    }

    fn book(bids: &[(f64, i64)], asks: &[(f64, i64)]) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels(
            "t",
            bids.iter()
                .map(|(p, s)| (Decimal::from_f64(*p).unwrap(), Decimal::from(*s)))
                .collect(),
            asks.iter()
                .map(|(p, s)| (Decimal::from_f64(*p).unwrap(), Decimal::from(*s)))
                .collect(),
            0,
        )
    }

    /// The audit's scenario: bid/ask = 0.49/0.51 with a 0.44 entry signal.
    /// No ask at or below 0.44 exists, so the bid could never trade — the old
    /// replay booked +0.421937 off exactly such a book.
    #[test]
    fn an_entry_needs_offer_depth_at_or_below_the_limit() {
        let m = market("t", "t-d");
        let mut rp = TwinReplay::new(
            Box::new(StubStrategy {
                token: "t".into(),
                entry: Some(dec!(0.44)),
                exit: false,
            }),
            &ExitConfig::default(),
        );

        // 0.49/0.51: the offer never reaches the 0.44 limit → no entry.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.51, 1000)]),
            1,
            880,
            1_000,
        ));
        assert_eq!(rp.open_positions(), 0, "no offer at the limit, no position");

        // An offer AT the limit but thinner than the position → still no fill.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.44, 5)]),
            1,
            880,
            2_000,
        ));
        assert_eq!(rp.open_positions(), 0, "5 offered shares cannot fill 10");

        // Full size at/below the limit → the resting bid fills.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.44, 10)]),
            1,
            880,
            3_000,
        ));
        assert_eq!(rp.open_positions(), 1, "10 offered at the limit fill 10");
    }

    /// Depth across levels counts too: 6 at 0.43 + 4 at 0.44 covers 10 shares
    /// at prices not worse than a 0.44 limit; moving one level out of the band
    /// leaves only 6.
    #[test]
    fn fillable_depth_accumulates_only_within_the_limit() {
        let m = market("t", "t-d");
        let mut rp = TwinReplay::new(
            Box::new(StubStrategy {
                token: "t".to_string(),
                entry: Some(dec!(0.44)),
                exit: false,
            }),
            &ExitConfig::default(),
        );

        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.44, 4), (0.43, 6)]),
            1,
            880,
            1_000,
        ));
        assert_eq!(rp.open_positions(), 1, "4 + 6 = 10 shares at <= 0.44 fill");

        let mut rp2 = TwinReplay::new(
            Box::new(StubStrategy {
                token: "t".to_string(),
                entry: Some(dec!(0.44)),
                exit: false,
            }),
            &ExitConfig::default(),
        );
        rp2.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.44, 4), (0.45, 6)]),
            1,
            880,
            1_000,
        ));
        assert_eq!(rp2.open_positions(), 0, "the 0.45 level is worse than the limit");
    }

    /// An exit only books against a REAL bid deep enough for the position. A
    /// close decided without one stays open (and settles at the round
    /// boundary) rather than being priced off a mid.
    #[test]
    fn an_exit_only_fills_against_a_deep_enough_bid() {
        let m = market("t", "t-d");
        let mut rp = TwinReplay::new(
            Box::new(StubStrategy {
                token: "t".to_string(),
                entry: Some(dec!(0.44)),
                exit: true,
            }),
            &ExitConfig::default(),
        );
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.44, 10)]),
            1,
            880,
            1_000,
        ));
        assert_eq!(rp.open_positions(), 1);

        // Close intent + no bid at all: the old code sold into the mid.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[], &[(0.51, 1000)]),
            1,
            880,
            2_000,
        ));
        assert_eq!(rp.open_positions(), 1, "no bid, no exit");
        assert_eq!(rp.closed_trades(), 0, "nothing booked off a bidless book");

        // A bid thinner than the position cannot absorb it either.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 5)], &[(0.51, 1000)]),
            1,
            880,
            3_000,
        ));
        assert_eq!(rp.open_positions(), 1, "5 bid shares cannot absorb 10");
        assert_eq!(rp.closed_trades(), 0);

        // A full-size bid books the exit at that bid.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.51, 1000)]),
            1,
            880,
            4_000,
        ));
        assert_eq!(rp.open_positions(), 0);
        assert_eq!(rp.closed_trades(), 1);
        let trades = rp.windowed_trades(10_000, 5_000);
        let expected_fee =
            taker_fee_pct(dec!(0.49)) / Decimal::ONE_HUNDRED * dec!(0.49) * TWIN_SHARES;
        assert_eq!(
            trades[0].1,
            (dec!(0.49) - dec!(0.44)) * TWIN_SHARES - expected_fee,
            "exit fills at the bid, not at a mid"
        );
    }

    /// A position whose market leaves the round is SETTLED, never deleted:
    /// at the last known executable bid when there is one.
    #[test]
    fn a_round_dropped_position_settles_at_its_last_known_bid() {
        let m = market("t", "t-d");
        let mut rp = TwinReplay::new(
            Box::new(StubStrategy {
                token: "t".to_string(),
                entry: Some(dec!(0.44)),
                exit: false,
            }),
            &ExitConfig::default(),
        );
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.49, 100)], &[(0.44, 10)]),
            1,
            880,
            1_000,
        ));
        // Keep the position open (no exit intent; -9% is far from the 12% stop)
        // while the book prints a fresh executable bid.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[(0.40, 100)], &[(0.42, 100)]),
            1,
            870,
            2_000,
        ));
        assert_eq!(rp.open_positions(), 1);

        // New round without the token: settle at the last known bid (0.40).
        let next = market("u", "d");
        rp.on_round(std::slice::from_ref(&next), &[], 5_000);
        assert_eq!(rp.open_positions(), 0);
        assert_eq!(rp.closed_trades(), 1);
        let trades = rp.windowed_trades(10_000, 6_000);
        let expected_fee =
            taker_fee_pct(dec!(0.40)) / Decimal::ONE_HUNDRED * dec!(0.40) * TWIN_SHARES;
        assert_eq!(
            trades[0].1,
            (dec!(0.40) - dec!(0.44)) * TWIN_SHARES - expected_fee,
            "the dropped position books its exit at the last executable bid"
        );
    }

    /// Without ANY known executable bid the settlement must not invent a
    /// price: the full entry cost is booked as an unrealized-principal loss
    /// and stays in the statistics.
    #[test]
    fn a_round_dropped_position_without_a_bid_books_a_full_principal_loss() {
        let m = market("t", "t-d");
        let mut rp = TwinReplay::new(
            Box::new(StubStrategy {
                token: "t".to_string(),
                entry: Some(dec!(0.44)),
                exit: false,
            }),
            &ExitConfig::default(),
        );
        // Enter against a bidless book: the offer side is what fills the bid.
        rp.on_tick(&tick_ctx(
            std::slice::from_ref(&m),
            "t",
            &book(&[], &[(0.44, 10)]),
            1,
            880,
            1_000,
        ));
        assert_eq!(rp.open_positions(), 1);
        // No tick ever showed a bid → no settlement price exists.
        let next = market("u", "d");
        rp.on_round(std::slice::from_ref(&next), &[], 5_000);
        assert_eq!(rp.open_positions(), 0);
        assert_eq!(rp.closed_trades(), 1, "the loss is recorded, not dropped");
        let trades = rp.windowed_trades(10_000, 6_000);
        assert_eq!(trades[0].1, dec!(-4.4), "full entry cost booked as a loss");
    }
}
