//! The `pair_arb` strategy: COMPLETE-SET PAIR ARBITRAGE on one binary market.
//!
//! One UP share and one DOWN share of the same market are a COMPLETE SET: they
//! pay exactly $1 at expiry whichever side wins, so a set bought for less than
//! $1 is a locked-in profit that no price move can take away. This strategy
//! builds the set on the MAKER side — it rests a BUY at each leg's best bid —
//! and holds the pair to settlement, where the kernel redeems both legs
//! (winner $1 / loser $0) instead of selling them on the exit ladder. There is
//! no directional view anywhere in the logic.
//!
//! # ⚠ MEASURED UNPROFITABLE — do not enable this live
//!
//! The locked discount is real but far too small to pay for what a failed
//! completion costs. Over a 34-hour frozen corpus (401 markets, dry replay,
//! group-level accounting by `conditionId`), every configuration tested lost
//! money:
//!
//! ```text
//! config                     entries  complete   group WR   group PF      net
//! naked_stop_sec=15 (dflt)       297     32.0%      34.7%      0.161   -$53.01
//! naked_stop_sec=0                60     65.0%      73.3%      0.439   -$21.84
//! min_edge=0.02, stop=0          127     79.5%      85.0%      0.659   -$14.55
//! min_edge=0.02, t600s, stop=0    42     66.7%      76.2%      0.579    -$9.10
//! ```
//!
//! The group WIN RATE target (≥75%) is reachable — 85.0% above, because a
//! completed pair is a guaranteed small win. The group PROFIT FACTOR target
//! (≥4) is not: the best measured PF is 0.659. The reason is the naked column,
//! and it is not bad luck.
//!
//! ## Why it fails: the maker fill is adversely selected
//!
//! A leg fills precisely when the market moves against that side, and the
//! sibling then does NOT fill — so the leg left in hand is the one already
//! moving away. Measured over the same corpus, a naked leg won only **4.5%** of
//! the time at the shipped setting (23.8%–28.6% with the stop off): not the
//! coin flip the "bought at the bid, so +half-spread EV" argument predicts.
//! Meanwhile a completed pair pays `shares × (1 − bid_sum)`, about **1–2% of
//! one leg**, while one naked leg costs up to **100%** of one leg. That
//! arithmetic caps the profit factor near 1 whatever the completion rate does,
//! and the measurement lands below 1 in every configuration.
//!
//! This also explains the ask-side absence below: if resting a complete set
//! below a dollar were free money, the market makers minting at par would be
//! resting there too. They are not. The sub-dollar BID sum is not a discount
//! somebody left on the table; it is the compensation for the adverse
//! selection, and the measurement says it does not cover it.
//!
//! The defaults were deliberately NOT tuned to the best row above: picking a
//! winner from a five-way sweep over one 34-hour corpus is curve-fitting, not
//! evidence. `pair_arb` is kept as a RESEARCH INSTRUMENT — it is registered at
//! startup but never auto-enabled, so nothing trades it unless an operator asks.
//!
//! ## Why the maker side, and not "buy the asks"
//!
//! The obvious version of this trade — buy both legs at their asks whenever
//! `up_ask + down_ask < 1` — **does not exist on this venue**. Measured over
//! 7.5 days of archived books (~800k instants where both legs of a market are
//! quoted simultaneously):
//!
//! ```text
//! complete-set ASK floor  = 1.0000  (p1 = 1.0010, median = 1.0100)
//! complete-set BID ceiling = 1.0000  (p50 = 0.9900, p95 = 0.9950)
//! ```
//!
//! The ask floor at exactly $1.00 is market makers minting and redeeming sets
//! at par; a taker can never buy a complete set below it, so the
//! buy-both-at-the-ask arbitrage is structurally absent. (Early per-event scans
//! "found" thousands of sub-dollar sums; every one of them paired a fresh quote
//! on one leg with the sibling's 1.3-second-old quote. Of 278 such dislocations
//! in the archive, none survived the next snapshot, 149 were gone within a
//! millisecond, and 129 never reappeared — they were dead quotes, not prices.)
//!
//! What the same data DOES show is the mirror image: the bid sum sits at a
//! median of 0.9900, i.e. a complete set can be *bought by resting* at both
//! bids for about $0.99 and redeemed at $1.00. A maker fill pays no fee at all
//! (the kernel charges `taker_fee_pct` only to crossing fills), so the whole
//! 1% is kept. That is the trade this strategy implements.
//!
//! ## The entry, precisely
//!
//! A pair is entered only when the resting prices give a locked edge:
//!
//! ```text
//! 1 − (up_bid + down_bid) ≥ min_edge
//! ```
//!
//! Both legs are emitted in ONE `evaluate` (the kernel's `pending_tokens`
//! allows a single entry per token per round, so a "second leg later" is not
//! expressible — the pair must be born complete). Sizes are EQUAL in shares so
//! the set is complete by construction, and the declared count is what the
//! kernel uses, clamped to its own risk band.
//!
//! ## Completion, and what happens when it fails
//!
//! A resting bid fills when that leg's ask prints down to it — the kernel's own
//! `Book::crosses` rule — fee-free. Both legs filling is the payoff. A leg that
//! never fills leaves the other NAKED, and that is where the money goes: over
//! the 34-hour corpus the completion rate was 32.0% at the shipped 15s stop and
//! 65.0% with the stop off, and the naked legs that resulted were the loss
//! centre (see the header). A naked leg pays directionally; the strategy does
//! not pretend to have an edge there. `naked_stop_sec` bounds the damage by
//! closing a leg that is still alone this long after its partner filled, rather
//! than carrying it to settlement. (Set `naked_stop_sec` to 0 to hold instead —
//! measured better per group, because the stop realizes the adverse selection
//! immediately, but worse per entry, because closing frees the slot for another
//! entry that is also negative-expectancy. Neither is good; see the header.)
//!
//! ## Shared gates this strategy waives, and why
//!
//! `gate_exemptions` declares `timing` and `momentum`:
//!  - `momentum` is a DIRECTIONAL filter (spot moving against the bet). A pair
//!    bets both directions at once, so the gate can only ever block one leg and
//!    let the other through — it would manufacture the naked leg this strategy
//!    exists to avoid.
//!  - `timing` is replaced by this strategy's own time discipline, measured
//!    against the MARKET's `expires_at_ms`, and the declared floor
//!    (`timing_min_time_left_sec`) is the same number, so the exemption can
//!    never reach into the force-exit window.
//!
//! ## Honest limits
//!
//!  - A maker fill is not guaranteed, and the measured completion rate (32.0%
//!    at the shipped stop, 65.0% with it off) is what the archive shows, not a
//!    promise. It is measured on fills the DRY matcher grants whenever a quote
//!    crosses; a live queue can be worse.
//!  - The naked leg is adversely selected, not a coin flip. This is the single
//!    finding that kills the strategy, and it is the one number a live run
//!    would need to confirm before anyone revisited this.
//!  - Capital is locked until expiry. `notional_cap_usd` bounds how much is
//!    locked per pair, and the kernel's own `max_shares` caps the size again.
//!  - The hold-to-settlement declaration is honoured where the kernel settles
//!    locally (dry/read-only), which is where this strategy is measured. On a
//!    live kernel the exit ladder keeps running, so a live pair is not carried
//!    to expiry yet.

use blitzkrieg_strategy_api::{
    BookUpdate, Break, Entry, Exit, FreshBook, Intents, Knob, ParamBag, RoundContext, SafeStrategy,
    dec as parse_dec, export_strategy,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::HashMap;

/// One side's top of book. The BID is what this strategy rests at; the ask
/// rides along for the diagnostics view.
#[derive(Debug, Clone, Copy, Default)]
struct Top {
    bid: Decimal,
    ask: Decimal,
}

/// The pair's economics for one market. `None` from [`PairArb::quote`] means
/// a side has no usable bid, i.e. the market cannot carry a resting pair.
#[derive(Debug, Clone, Copy)]
struct PairQuote {
    up_bid: Decimal,
    down_bid: Decimal,
    bid_sum: Decimal,
    /// `1 − bid_sum`: the locked edge per share-pair if both legs fill as
    /// makers. No fee term — a maker fill is fee-free by construction.
    edge: Decimal,
    shares: Decimal,
}

/// Tunables. Every one of them is also a hot-bag knob (`on_params`) so Shadow
/// Evolution can move it inside its declared domain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PairConfig {
    /// Minimum locked-in edge per share-pair, in dollars, with both legs as
    /// MAKER fills.
    min_edge: Decimal,
    /// Capital cap per pair, in dollars: `n = floor(cap / bid_sum)` shares per
    /// leg, so the pair's total cost stays inside the cap.
    notional_cap_usd: Decimal,
    /// Never enter a market with less than this much time to its expiry: a
    /// pair that fails to complete needs time left to be resolved.
    min_time_left_sec: i64,
    /// Close a leg that is still naked this many seconds after its partner
    /// filled. 0 disables the stop (hold the naked leg to settlement).
    naked_stop_sec: i64,
}

impl Default for PairConfig {
    fn default() -> Self {
        Self {
            // One cent per pair: the measured median locked edge is exactly
            // 0.0100, and a bar below it would admit the noise around it.
            min_edge: dec!(0.010),
            notional_cap_usd: dec!(5),
            min_time_left_sec: 300,
            // 15s was chosen from a second-leg-delay distribution (p50 1.4-3.9s,
            // p90 5.5-10.5s) on the assumption it would keep nearly every
            // completion. The measurement disagreed: completion came out at
            // 32.0% with the stop on against 65.0% with it off, because a
            // partner that is about to fill is exactly the partner this stop
            // cancels. Kept as shipped rather than retuned to the best sweep row
            // — see the header on why tuning here would be curve-fitting.
            naked_stop_sec: 15,
        }
    }
}

/// The markets of the last evaluation cycle. `diagnostics` receives no
/// context, so the pair view it reports is built from this cache.
#[derive(Debug, Clone)]
struct MarketRow {
    condition_id: String,
    up_token: String,
    down_token: String,
}

/// One pair in flight, as this strategy last saw it. The kernel does not push
/// fills to the strategy, so "did my partner leg fill" is inferred from the
/// book: a leg whose ask has printed down to the resting price is exactly the
/// condition the kernel's maker matcher uses.
#[derive(Debug, Clone, Copy)]
struct PairState {
    slot: i64,
    up_price: Decimal,
    down_price: Decimal,
    /// When each leg's resting price was first crossed by that leg's ask, i.e.
    /// when it filled (or would have). `None` = still resting.
    up_filled_ms: Option<i64>,
    down_filled_ms: Option<i64>,
    /// Set once the naked-leg stop has been emitted for this pair.
    stopped: bool,
}

struct PairArb {
    cfg: PairConfig,
    /// This cycle's priceable books: FRESH rows only, as the host judged them.
    eval_books: HashMap<String, Top>,
    /// condition_id → the pair emitted in that market's round. At most ONE pair
    /// per market per round: a persistent edge cannot stack exposure all round.
    entered: HashMap<String, PairState>,
    /// Markets seen in the last `evaluate` (diagnostics has no ctx).
    markets: Vec<MarketRow>,
    /// Round slot in force; a change resets `entered`.
    slot: i64,
    /// The last now_ms seen in `evaluate`, used to date the inferred fills.
    now_ms: i64,
}

impl Default for PairArb {
    fn default() -> Self {
        Self {
            cfg: PairConfig::default(),
            eval_books: HashMap::new(),
            entered: HashMap::new(),
            markets: Vec::new(),
            slot: i64::MIN,
            now_ms: 0,
        }
    }
}

impl PairArb {
    /// Price the pair off the BIDS, or `None` when either side has no bid to
    /// rest at. The edge carries no fee term: both legs are maker fills.
    fn quote(&self, up: Top, down: Top) -> Option<PairQuote> {
        if up.bid <= Decimal::ZERO || down.bid <= Decimal::ZERO {
            return None;
        }
        let bid_sum = up.bid + down.bid;
        let edge = Decimal::ONE - bid_sum;
        // Equal SHARES on both legs, so the pair is complete by construction.
        // The cap divides by the PAIR's cost (both legs together), and a count
        // below one share means the cap cannot buy a complete set here.
        let shares = (self.cfg.notional_cap_usd / bid_sum).floor();
        Some(PairQuote {
            up_bid: up.bid,
            down_bid: down.bid,
            bid_sum,
            edge,
            shares,
        })
    }

    /// The share count for a pair: the capital cap's floor, never below one
    /// share (a count below one buys a naked leg). The kernel clamps the
    /// declared count to its own `max_shares` band before placing.
    fn shares_for(&self, q: &PairQuote) -> Option<Decimal> {
        let n = q.shares.floor();
        if n < Decimal::ONE {
            return None;
        }
        Some(n)
    }

    /// Track the pair's legs against this cycle's books: a leg whose ask has
    /// printed down to its resting price is a maker fill under the kernel's own
    /// `Book::crosses` rule.
    fn observe_pair(&mut self, cid: &str, m: &blitzkrieg_strategy_api::MarketInfo) {
        let Some(state) = self.entered.get(cid).copied() else {
            return;
        };
        if state.slot != m.slot {
            return;
        }
        let mut next = state;
        if next.up_filled_ms.is_none()
            && let Some(up) = self.eval_books.get(&m.up_token)
            && up.ask > Decimal::ZERO
            && up.ask <= next.up_price
        {
            next.up_filled_ms = Some(self.now_ms);
        }
        if next.down_filled_ms.is_none()
            && let Some(down) = self.eval_books.get(&m.down_token)
            && down.ask > Decimal::ZERO
            && down.ask <= next.down_price
        {
            next.down_filled_ms = Some(self.now_ms);
        }
        self.entered.insert(cid.to_string(), next);
    }

    /// The naked-leg stop: when exactly one leg has filled and the other has
    /// not by `naked_stop_sec` after it, close the filled leg at the live bid.
    fn naked_stop_intents(&mut self, ctx: &RoundContext, it: &mut Intents) {
        if self.cfg.naked_stop_sec <= 0 {
            return;
        }
        let grace_ms = self.cfg.naked_stop_sec * 1000;
        let mut stop: Vec<(String, String, Decimal)> = Vec::new();
        for m in &ctx.markets {
            let Some(state) = self.entered.get(&m.condition_id).copied() else {
                continue;
            };
            if state.slot != m.slot || state.stopped {
                continue;
            }
            // (the FILLED leg, its token, the UNFILLED partner, the partner's
            // resting price).
            let (filled, token, partner, partner_price) =
                match (state.up_filled_ms, state.down_filled_ms) {
                    (Some(t), None) => (
                        t,
                        m.up_token.clone(),
                        m.down_token.clone(),
                        state.down_price,
                    ),
                    (None, Some(t)) => {
                        (t, m.down_token.clone(), m.up_token.clone(), state.up_price)
                    }
                    _ => continue,
                };
            if self.now_ms - filled < grace_ms {
                continue;
            }
            if let Some(state) = self.entered.get_mut(&m.condition_id) {
                state.stopped = true;
            }
            stop.push((token, partner, partner_price));
        }
        for (token, partner, partner_price) in stop {
            it.exits.push(Exit {
                token,
                reason: "naked leg: partner never filled".into(),
            });
            // The partner's resting bid is STILL LIVE when the stop fires. Left
            // alone it can fill after the stop — XRP 0xb154 did exactly that in
            // the first honest replay, leaving a fresh naked leg on the other
            // side that nothing was watching. The break cancels it, so the pair
            // ends closed rather than half-open in the opposite direction.
            it.breaks.push(Break {
                token: partner,
                broken_price: partner_price.normalize().to_string(),
            });
        }
    }
}

impl SafeStrategy for PairArb {
    fn name(&self) -> &str {
        "pair_arb"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }

    /// Deliberately inert: this strategy prices EXCLUSIVELY off the host's
    /// fresh rows in `on_eval_books`, because an arb leg priced off a stale
    /// book is a losing leg. A host that never binds an evaluation context
    /// therefore leaves this strategy inert rather than mispriced.
    fn on_book(&mut self, _update: &BookUpdate) {}

    fn on_eval_books(&mut self, books: &[FreshBook]) {
        // The fresh rows REPLACE the view wholesale each cycle: a token
        // without a fresh row is simply absent, and stale and missing are
        // indistinguishable — the same semantics the kernel gives an in-tree
        // strategy through `StrategyCtx::fresh_book`.
        self.eval_books = books
            .iter()
            .filter(|fb| fb.fresh)
            .filter_map(|fb| {
                let bid = parse_dec(&fb.book.best_bid)?;
                if bid <= Decimal::ZERO {
                    return None;
                }
                let ask = parse_dec(&fb.book.best_ask).unwrap_or(Decimal::ZERO);
                Some((fb.book.symbol.clone(), Top { bid, ask }))
            })
            .collect();
    }

    fn evaluate(&mut self, ctx: &RoundContext) -> Intents {
        let mut it = Intents::none();
        self.now_ms = ctx.round.now_ms;
        if ctx.round.slot != self.slot {
            self.slot = ctx.round.slot;
            self.entered.clear();
        }
        self.markets = ctx
            .markets
            .iter()
            .map(|m| MarketRow {
                condition_id: m.condition_id.clone(),
                up_token: m.up_token.clone(),
                down_token: m.down_token.clone(),
            })
            .collect();

        // Track the pairs already in flight first, so the naked-leg stop sees
        // this cycle's fills before entries are considered.
        for m in &ctx.markets {
            self.observe_pair(&m.condition_id, m);
        }
        self.naked_stop_intents(ctx, &mut it);

        for m in &ctx.markets {
            if self.entered.contains_key(&m.condition_id) {
                continue;
            }
            let (Some(&up), Some(&down)) = (
                self.eval_books.get(&m.up_token),
                self.eval_books.get(&m.down_token),
            ) else {
                continue;
            };
            // The MARKET's own expiry is the authority on time left; a market
            // that does not state one falls back to the round clock.
            let time_left_ms = if m.expires_at_ms > 0 {
                m.expires_at_ms - ctx.round.now_ms
            } else {
                ctx.round.time_left_sec * 1000
            };
            if time_left_ms < self.cfg.min_time_left_sec * 1000 {
                continue;
            }
            let Some(q) = self.quote(up, down) else {
                continue;
            };
            if q.edge < self.cfg.min_edge {
                continue;
            }
            let Some(n) = self.shares_for(&q) else {
                continue;
            };
            let n = n.normalize().to_string();
            let reason = format!(
                "complete-set maker pair: {} + {} = {} → edge ${} per pair ×{n}",
                q.up_bid, q.down_bid, q.bid_sum, q.edge
            );
            // Both legs, same cycle, same declared share count, resting at the
            // bids. The kernel places them back-to-back and lets the second
            // past the one-position-per-asset gate as a pair completion.
            it.entries.push(Entry {
                token: m.up_token.clone(),
                price: q.up_bid.normalize().to_string(),
                reason: reason.clone(),
                shares: Some(n.clone()),
            });
            it.entries.push(Entry {
                token: m.down_token.clone(),
                price: q.down_bid.normalize().to_string(),
                reason,
                shares: Some(n),
            });
            self.entered.insert(
                m.condition_id.clone(),
                PairState {
                    slot: m.slot,
                    up_price: q.up_bid,
                    down_price: q.down_bid,
                    up_filled_ms: None,
                    down_filled_ms: None,
                    stopped: false,
                },
            );
        }
        it
    }

    fn diagnostics(&self) -> Vec<serde_json::Value> {
        self.markets
            .iter()
            .map(|m| {
                let q = match (
                    self.eval_books.get(&m.up_token).copied(),
                    self.eval_books.get(&m.down_token).copied(),
                ) {
                    (Some(u), Some(d)) => self.quote(u, d),
                    _ => None,
                };
                let st = self.entered.get(&m.condition_id);
                serde_json::json!({
                    "conditionId": m.condition_id,
                    "upBid": self.eval_books.get(&m.up_token).map(|t| t.bid.to_string()),
                    "upAsk": self.eval_books.get(&m.up_token).map(|t| t.ask.to_string()),
                    "downBid": self.eval_books.get(&m.down_token).map(|t| t.bid.to_string()),
                    "downAsk": self.eval_books.get(&m.down_token).map(|t| t.ask.to_string()),
                    "bidSum": q.map(|q| q.bid_sum.to_string()),
                    "edge": q.map(|q| q.edge.to_string()),
                    "shares": q.map(|q| q.shares.to_string()),
                    "enteredSlot": st.map(|s| s.slot),
                    "upFilledMs": st.and_then(|s| s.up_filled_ms),
                    "downFilledMs": st.and_then(|s| s.down_filled_ms),
                })
            })
            .collect()
    }

    /// Hot-bag knobs (Shadow Evolution) arrive as this strategy's own
    /// snake_case names at the top level. The kernel's legacy `spreadArb`
    /// config package belongs to the dip buyer and is deliberately NOT read
    /// here — its fields have no meaning for a pair.
    fn on_params(&mut self, p: &ParamBag) -> bool {
        let mut cfg = self.cfg.clone();
        if let Some(d) = p.get_dec("min_edge")
            && d >= Decimal::ZERO
        {
            cfg.min_edge = d;
        }
        if let Some(d) = p.get_dec("notional_cap_usd")
            && d > Decimal::ZERO
        {
            cfg.notional_cap_usd = d;
        }
        if let Some(secs) = p
            .get_dec("min_time_left_sec")
            .and_then(|d| d.trunc().to_i64())
            && secs >= 0
        {
            cfg.min_time_left_sec = secs;
        }
        if let Some(secs) = p.get_dec("naked_stop_sec").and_then(|d| d.trunc().to_i64())
            && secs >= 0
        {
            cfg.naked_stop_sec = secs;
        }
        self.cfg = cfg;
        true
    }

    fn evolvable_knobs(&self) -> Vec<Knob> {
        // The PERFORMANCE knobs. `notional_cap_usd` is a capital knob, not a
        // performance one (it scales every result alike), so it stays out of
        // the evolvable set while remaining settable through `on_params`.
        vec![
            Knob {
                name: "min_edge".into(),
                value: self.cfg.min_edge.to_string(),
                min: "0.005".into(),
                max: "0.05".into(),
            },
            Knob {
                name: "min_time_left_sec".into(),
                value: self.cfg.min_time_left_sec.to_string(),
                min: "180".into(),
                max: "840".into(),
            },
            Knob {
                name: "naked_stop_sec".into(),
                value: self.cfg.naked_stop_sec.to_string(),
                min: "0".into(),
                max: "300".into(),
            },
        ]
    }

    fn config_view(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "minEdge": self.cfg.min_edge.to_string(),
            "notionalCapUsd": self.cfg.notional_cap_usd.to_string(),
            "minTimeLeftSec": self.cfg.min_time_left_sec,
            "nakedStopSec": self.cfg.naked_stop_sec,
        }))
    }

    /// See the module docs: `momentum` is directional and would split the
    /// pair; `timing` is replaced by this strategy's own market-expiry gate,
    /// with the declared floor keeping the exemption out of the force-exit
    /// window.
    fn gate_exemptions(&self) -> &'static [&'static str] {
        &["timing", "momentum"]
    }

    fn timing_min_time_left_sec(&self) -> Option<i64> {
        Some(self.cfg.min_time_left_sec)
    }

    /// A complete set redeemed at expiry is the payoff; selling a leg early
    /// turns it back into a directional trade. The naked-leg stop is an
    /// explicit `Exit` intent, which the kernel honours even for a
    /// hold-to-settlement strategy.
    fn holds_to_settlement(&self) -> bool {
        true
    }
}

export_strategy!(crate::PairArb);

#[cfg(test)]
mod tests {
    use super::*;
    use blitzkrieg_strategy_api::{MarketInfo, RoundInfo};

    const ROUND_MS: i64 = 900_000;

    fn fresh(token: &str, bid: &str, ask: &str) -> FreshBook {
        FreshBook {
            book: BookUpdate {
                symbol: token.into(),
                best_bid: Some(bid.into()),
                best_ask: Some(ask.into()),
                ..Default::default()
            },
            fresh: true,
        }
    }

    fn market(now_ms: i64) -> MarketInfo {
        let slot = now_ms / ROUND_MS;
        MarketInfo {
            asset: "BTC".into(),
            condition_id: "cond".into(),
            up_token: "up".into(),
            down_token: "down".into(),
            expires_at_ms: (slot + 1) * ROUND_MS,
            slot,
            neg_risk: true,
        }
    }

    fn ctx(now_ms: i64) -> RoundContext {
        let m = market(now_ms);
        RoundContext {
            round: RoundInfo {
                slot: m.slot,
                time_left_sec: (m.expires_at_ms - now_ms) / 1000,
                now_ms,
            },
            markets: vec![m],
        }
    }

    /// A strategy holding fresh books at the given bids; asks sit one tick up.
    fn armed(up_bid: &str, down_bid: &str) -> PairArb {
        let mut s = PairArb::default();
        s.on_eval_books(&[fresh("up", up_bid, "0.99"), fresh("down", down_bid, "0.99")]);
        s
    }

    /// 800s to expiry of a 900s round: comfortably inside the default gate.
    const NOW: i64 = 1_000_000;

    #[test]
    fn a_discounted_set_is_rested_at_both_bids_with_equal_shares() {
        // 0.44 + 0.55 = 0.99 → edge 0.01; shares = floor(5 / 0.99) = 5.
        let mut s = armed("0.44", "0.55");
        let it = s.evaluate(&ctx(NOW));
        assert_eq!(it.entries.len(), 2, "a pair is two legs, in one cycle");
        assert_eq!(it.entries[0].token, "up");
        assert_eq!(it.entries[0].price, "0.44", "the leg rests at ITS OWN bid");
        assert_eq!(it.entries[0].shares.as_deref(), Some("5"));
        assert_eq!(it.entries[1].token, "down");
        assert_eq!(it.entries[1].price, "0.55");
        assert_eq!(
            it.entries[1].shares, it.entries[0].shares,
            "both legs must match in SHARES or the set is not complete"
        );
        assert!(it.entries[0].reason.contains("complete-set maker pair"));
    }

    #[test]
    fn no_pair_when_the_bids_leave_no_edge() {
        // 0.50 + 0.50 = 1.00: nothing locked, so nothing to rest.
        let mut s = armed("0.50", "0.50");
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty());
        // 0.49 + 0.50 = 0.99: a cent of edge, exactly at the default bar.
        let mut s = armed("0.49", "0.50");
        assert_eq!(s.evaluate(&ctx(NOW)).entries.len(), 2);
        // Half a cent is below the bar.
        let mut s = armed("0.495", "0.50");
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty());
    }

    #[test]
    fn no_pair_without_a_fresh_book_on_both_sides() {
        let mut s = PairArb::default();
        s.on_eval_books(&[fresh("up", "0.44", "0.46")]);
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty(), "down leg missing");
        // A stale row is indistinguishable from a missing one.
        let mut s = PairArb::default();
        let mut down = fresh("down", "0.55", "0.57");
        down.fresh = false;
        s.on_eval_books(&[fresh("up", "0.44", "0.46"), down]);
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty(), "down leg stale");
    }

    #[test]
    fn a_market_is_entered_at_most_once_per_round() {
        let mut s = armed("0.44", "0.55");
        assert_eq!(s.evaluate(&ctx(NOW)).entries.len(), 2);
        assert!(
            s.evaluate(&ctx(NOW)).entries.is_empty(),
            "the same edge in the same round must not stack a second pair"
        );
        // Next round: new market, new books, a fresh pair is allowed.
        let next = 2 * ROUND_MS + 60_000;
        s.on_eval_books(&[fresh("up", "0.44", "0.46"), fresh("down", "0.55", "0.57")]);
        assert_eq!(s.evaluate(&ctx(next)).entries.len(), 2);
    }

    #[test]
    fn the_min_time_left_gate_blocks_a_late_entry() {
        // 100s to expiry, default floor is 300s.
        let late = ROUND_MS - 100_000;
        let mut s = armed("0.44", "0.55");
        assert!(s.evaluate(&ctx(late)).entries.is_empty());
        // A floor below the remaining time lets the same pair through: the
        // gate is the knob, not a fixed rule.
        s.cfg.min_time_left_sec = 60;
        assert_eq!(s.evaluate(&ctx(late)).entries.len(), 2);
    }

    #[test]
    fn the_capital_cap_sets_the_share_count() {
        let mut s = armed("0.44", "0.55");
        s.cfg.notional_cap_usd = dec!(1);
        let it = s.evaluate(&ctx(NOW));
        assert_eq!(it.entries[0].shares.as_deref(), Some("1"), "floor(1/0.99)");
        // A cap that cannot buy one complete set buys nothing at all.
        let mut s = armed("0.44", "0.55");
        s.cfg.notional_cap_usd = dec!(0.9);
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty());
    }

    #[test]
    fn hot_params_move_the_knobs_and_the_gate_follows() {
        let mut s = armed("0.44", "0.55");
        let mut bag = ParamBag::default();
        bag.0.insert("min_edge".into(), "0.03".into());
        bag.0.insert("notional_cap_usd".into(), "1".into());
        assert!(s.on_params(&bag));
        // The 0.01 edge no longer clears the raised bar.
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty());
        let mut bag = ParamBag::default();
        bag.0.insert("min_edge".into(), "0.01".into());
        bag.0.insert("notional_cap_usd".into(), "1".into());
        assert!(s.on_params(&bag));
        let it = s.evaluate(&ctx(NOW));
        assert_eq!(it.entries.len(), 2);
        assert_eq!(it.entries[0].shares.as_deref(), Some("1"));
    }

    #[test]
    fn malformed_book_rows_are_ignored() {
        let mut s = PairArb::default();
        s.on_eval_books(&[
            fresh("up", "not-a-price", "0.46"),
            fresh("down", "0.55", "0.57"),
        ]);
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty());
        // A zero bid is not a price either.
        let mut s = PairArb::default();
        s.on_eval_books(&[fresh("up", "0", "0.46"), fresh("down", "0.55", "0.57")]);
        assert!(s.evaluate(&ctx(NOW)).entries.is_empty());
    }

    #[test]
    fn a_filled_leg_tracks_its_fill_from_the_book() {
        let mut s = armed("0.44", "0.55");
        assert_eq!(s.evaluate(&ctx(NOW)).entries.len(), 2);
        // UP's ask prints down to the resting bid: that leg has filled.
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.55", "0.57")]);
        let it = s.evaluate(&ctx(NOW + 1_000));
        assert!(it.entries.is_empty(), "no new pair while one is in flight");
        let st = s.entered.get("cond").copied().unwrap();
        assert_eq!(st.up_filled_ms, Some(NOW + 1_000));
        assert_eq!(st.down_filled_ms, None);
    }

    #[test]
    fn the_naked_stop_closes_a_lone_leg_after_the_grace_period() {
        let mut s = armed("0.44", "0.55");
        assert_eq!(s.evaluate(&ctx(NOW)).entries.len(), 2);
        // UP fills, DOWN never does.
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.55", "0.57")]);
        s.evaluate(&ctx(NOW + 1_000));
        // Inside the grace period: nothing yet.
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.55", "0.57")]);
        let inside = s.evaluate(&ctx(NOW + 10_000));
        assert!(inside.exits.is_empty());
        assert!(inside.breaks.is_empty());
        // Past it: the filled leg is closed at the live bid, and the partner's
        // still-resting bid is cancelled in the same cycle — otherwise it could
        // fill afterwards and leave a fresh naked leg nothing is watching.
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.55", "0.57")]);
        let it = s.evaluate(&ctx(NOW + 20_000));
        assert_eq!(it.exits.len(), 1);
        assert_eq!(it.exits[0].token, "up", "the FILLED leg is the one closed");
        assert!(it.exits[0].reason.contains("naked"));
        assert_eq!(it.breaks.len(), 1);
        assert_eq!(
            it.breaks[0].token, "down",
            "the UNFILLED partner is cancelled"
        );
        assert_eq!(it.breaks[0].broken_price, "0.55");
        // Emitted once, not every cycle.
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.55", "0.57")]);
        let after = s.evaluate(&ctx(NOW + 21_000));
        assert!(after.exits.is_empty());
        assert!(after.breaks.is_empty());
    }

    #[test]
    fn a_completed_pair_is_never_stopped() {
        let mut s = armed("0.44", "0.55");
        assert_eq!(s.evaluate(&ctx(NOW)).entries.len(), 2);
        // BOTH asks print down to the resting bids.
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.54", "0.55")]);
        s.evaluate(&ctx(NOW + 1_000));
        s.on_eval_books(&[fresh("up", "0.43", "0.44"), fresh("down", "0.54", "0.55")]);
        assert!(
            s.evaluate(&ctx(NOW + 120_000)).exits.is_empty(),
            "a complete set is redeemed at expiry, never sold"
        );
    }

    #[test]
    fn declarations_match_the_design() {
        let s = PairArb::default();
        assert_eq!(s.name(), "pair_arb");
        assert!(s.holds_to_settlement(), "a pair is redeemed, never sold");
        assert_eq!(s.gate_exemptions(), &["timing", "momentum"]);
        assert_eq!(
            s.timing_min_time_left_sec(),
            Some(s.cfg.min_time_left_sec),
            "the timing floor must be the strategy's own time gate"
        );
        let knobs = s.evolvable_knobs();
        let names: Vec<&str> = knobs.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["min_edge", "min_time_left_sec", "naked_stop_sec"]
        );
        assert!(s.config_view().is_some());
    }
}
