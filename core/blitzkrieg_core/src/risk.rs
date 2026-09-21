//! Minimal P0 risk gate that every order must pass inside the core.
//!
//! Full daily-loss / consecutive-loss / cooldown risk lands in P1/P2 with
//! positions. P0 enforces the invariants no order must ever bypass:
//!  - global kill switch — an ENTRY freeze: closing intents always pass
//!  - per-order notional cap
//!  - (the ledger independently prevents overspending)

use crate::model::{CoreError, CoreErrorCode, CoreResult, OrderRequest, Side};
use rust_decimal::Decimal;
use std::collections::HashMap;

/// The internal-key prefixes that name a CLOSING intent: an automated exit, a
/// strategy close, a manual flatten or the residual backstop. This is the ONE
/// definition — the placement path (`service::is_close_intent`) delegates here,
/// because the risk gate and the retry policy must agree on what "closing"
/// means or one of them re-locks the escape hatch the other opened (P0 #174).
pub fn is_close_intent(internal_key: &str) -> bool {
    internal_key.starts_with("exit:")
        || internal_key.starts_with("exit-strategy:")
        || internal_key.starts_with("flatten:")
        || internal_key.starts_with("exit-residual:")
}

/// True when this request only REDUCES exposure. The kernel cannot see the
/// book from here, so a SELL is only trusted as a close when its intent key
/// says so; anything else stays subject to the entry freeze.
fn closes_exposure(req: &OrderRequest) -> bool {
    req.side == Side::Sell && is_close_intent(&req.internal_key)
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_order_notional: Decimal,
    /// P0 #202 — the same per-order bound, expressed as a PERCENTAGE of the
    /// account's cash equity at submission time. 0 = disabled (the shipped
    /// default: an unconfigured kernel behaves exactly as before).
    ///
    /// The absolute `max_order_notional` is a fixed number, so on a small book
    /// it is decoration rather than a bound: the supervisor derives 6.00 USD
    /// from `max_shares × 0.6`, which on the live 4.8 USDC account is 125% of
    /// it — and 10 shares at 0.60 (the smallest ticket the default share band
    /// produces) is already 125% of the book. A cap relative to the account is
    /// the only form that bounds "any one order" on every account size, which
    /// is what the operator actually asked for.
    pub max_order_notional_pct: Decimal,
    /// Portfolio-level cap on TOTAL open notional across all strategies
    /// (E16/#98). 0 = disabled (the shipped default); per-strategy caps are
    /// partitioned by `StrategyLimit.max_open_notional_usd`.
    pub max_open_notional_usd: Decimal,
    pub min_price: Decimal,
    pub max_price: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            // Mirrors the Node HFT defaults ($2.5/order, prices within the band).
            max_order_notional: Decimal::from(100),
            // Off by default: a shipped-behaviour change would need data.
            max_order_notional_pct: Decimal::ZERO,
            max_open_notional_usd: Decimal::ZERO,
            min_price: Decimal::ZERO,
            max_price: Decimal::ONE,
        }
    }
}

#[derive(Debug, Default)]
pub struct RiskGate {
    config: RiskConfig,
    killed: bool,
    kill_reason: Option<String>,
}

/// Consecutive-loss circuit breaker (mirrors the Node engine's MAX_CONSECUTIVE_LOSSES).
#[derive(Debug)]
pub struct LossBreaker {
    max_consecutive_losses: u32,
    cooldown_sec: i64,
    consecutive_losses: u32,
    halted_until_ms: i64,
}

impl LossBreaker {
    pub fn new(max_consecutive_losses: u32, cooldown_sec: i64) -> Self {
        Self {
            max_consecutive_losses,
            cooldown_sec,
            consecutive_losses: 0,
            halted_until_ms: 0,
        }
    }

    pub fn record(&mut self, net_pnl: Decimal, now_ms: i64) -> bool {
        if net_pnl < Decimal::ZERO {
            self.consecutive_losses += 1;
            if self.consecutive_losses >= self.max_consecutive_losses {
                self.halted_until_ms = now_ms + self.cooldown_sec * 1000;
                return true; // tripped
            }
        } else if net_pnl > Decimal::ZERO {
            self.consecutive_losses = 0;
        }
        false
    }

    pub fn is_halted(&self, now_ms: i64) -> bool {
        now_ms < self.halted_until_ms
    }

    /// Clear the halt if its cooldown elapsed; returns true if it just resumed.
    pub fn maybe_resume(&mut self, now_ms: i64) -> bool {
        if self.halted_until_ms > 0 && now_ms >= self.halted_until_ms {
            self.halted_until_ms = 0;
            self.consecutive_losses = 0;
            return true;
        }
        false
    }

    pub fn consecutive_losses(&self) -> u32 {
        self.consecutive_losses
    }
    pub fn halted_until_ms(&self) -> i64 {
        self.halted_until_ms
    }
}

/// Consecutive-loss breakers, ONE PER STRATEGY (KI-10 / D-18 option A).
///
/// A losing streak in one leg freezes only THAT leg's entries, so a
/// multi-strategy run stays attributable — with a single global breaker, any
/// strategy's streak silently vetoed every other strategy's candidates and the
/// out-of-sample numbers could not be read per strategy (see
/// `reports/TREND_FOLLOW_HOLDOUT_REPORT.md` §3.2).
///
/// The split is deliberately limited to the consecutive-loss breaker: the daily
/// loss cap and the kill switch stay GLOBAL, because they are account-level
/// constraints rather than per-strategy heuristics.
#[derive(Debug)]
pub struct LossBreakers {
    max_consecutive_losses: u32,
    cooldown_sec: i64,
    per_strategy: HashMap<String, LossBreaker>,
}

impl LossBreakers {
    pub fn new(max_consecutive_losses: u32, cooldown_sec: i64) -> Self {
        Self {
            max_consecutive_losses,
            cooldown_sec,
            per_strategy: HashMap::new(),
        }
    }

    /// Record a closed trade for `strategy`. Returns true when THIS strategy's
    /// breaker just tripped (its own cooldown starts now).
    pub fn record(&mut self, strategy: &str, net_pnl: Decimal, now_ms: i64) -> bool {
        let b = self
            .per_strategy
            .entry(strategy.to_string())
            .or_insert_with(|| LossBreaker::new(self.max_consecutive_losses, self.cooldown_sec));
        b.record(net_pnl, now_ms)
    }

    /// Is `strategy` currently halted? An unknown strategy is never halted —
    /// a breaker only exists once that leg has recorded a close.
    pub fn is_halted(&self, strategy: &str, now_ms: i64) -> bool {
        self.per_strategy
            .get(strategy)
            .is_some_and(|b| b.is_halted(now_ms))
    }

    pub fn halted_until_ms(&self, strategy: &str) -> i64 {
        self.per_strategy
            .get(strategy)
            .map_or(0, |b| b.halted_until_ms())
    }

    pub fn consecutive_losses(&self, strategy: &str) -> u32 {
        self.per_strategy
            .get(strategy)
            .map_or(0, |b| b.consecutive_losses())
    }

    /// Clear the halts whose cooldown elapsed; returns the strategies that just
    /// resumed, so the caller can log/report one resume per strategy.
    pub fn maybe_resume_all(&mut self, now_ms: i64) -> Vec<String> {
        let mut resumed = Vec::new();
        for (name, b) in self.per_strategy.iter_mut() {
            if b.maybe_resume(now_ms) {
                resumed.push(name.clone());
            }
        }
        resumed.sort();
        resumed
    }

    /// Strategies with an active halt at `now_ms` (observability/tests).
    pub fn halted(&self, now_ms: i64) -> Vec<String> {
        let mut v: Vec<String> = self
            .per_strategy
            .iter()
            .filter(|(_, b)| b.is_halted(now_ms))
            .map(|(name, _)| name.clone())
            .collect();
        v.sort();
        v
    }
}

impl RiskGate {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            killed: false,
            kill_reason: None,
        }
    }

    pub fn kill(&mut self, reason: impl Into<String>) {
        self.killed = true;
        self.kill_reason = Some(reason.into());
    }
    pub fn resume(&mut self) {
        self.killed = false;
        self.kill_reason = None;
    }
    pub fn is_killed(&self) -> bool {
        self.killed
    }
    /// Why trading is frozen, when it is.
    pub fn kill_reason(&self) -> Option<&str> {
        self.kill_reason.as_deref()
    }
    pub fn set_config(&mut self, c: RiskConfig) {
        self.config = c;
    }

    /// The live risk config (runtime updates land here, not in `CoreConfig`).
    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut RiskConfig {
        &mut self.config
    }

    pub fn check(&self, req: &OrderRequest) -> CoreResult<()> {
        self.check_with_equity(req, Decimal::ZERO)
    }

    /// [`RiskGate::check`] with the account's cash equity, which the
    /// equity-relative per-order cap (#202) is a percentage of. The caller that
    /// knows the balance is the one that holds the ledger, so the equity is
    /// passed in rather than remembered here: a stored copy can go stale, and a
    /// stale equity silently bounds a grown account by an old number.
    ///
    /// `equity_usd <= 0` means "unknown" and disables only the relative cap. An
    /// absent equity must not veto orders (the cap is a bound, and refusing
    /// everything because a caller did not say how big the account is would be
    /// the wrong failure); the wiring test in `service` is what keeps the live
    /// path from silently losing the cap.
    pub fn check_with_equity(&self, req: &OrderRequest, equity_usd: Decimal) -> CoreResult<()> {
        // The kill switch freezes NEW exposure, never the way OUT of it. A kill
        // is raised exactly when the book is most dangerous (venue refusals,
        // failed self-check, blind sweeps), so vetoing the closing SELL would
        // stretch the stop-limited risk into a total one: the escape hatch has
        // to stay open by construction (P0 #174). A genuine global stop of
        // trading is a process-level action (`service` stop path), not an order
        // rejection — subtracting the ability to close is not what "stopped"
        // means here.
        if self.killed && !closes_exposure(req) {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                self.kill_reason
                    .clone()
                    .unwrap_or_else(|| "kill switch active".into()),
            ));
        }
        if req.price <= self.config.min_price || req.price > self.config.max_price {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "price {} outside ({}..{}]",
                    req.price, self.config.min_price, self.config.max_price
                ),
            ));
        }
        if req.size <= Decimal::ZERO {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                "size must be positive",
            ));
        }
        // Notional cap applies to BUY commitment; SELL is bounded by position (P2).
        if req.side == Side::Buy {
            let notional = req.price * req.size;
            if notional > self.config.max_order_notional {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!(
                        "notional {notional} exceeds per-order cap {}",
                        self.config.max_order_notional
                    ),
                ));
            }
        }
        // P0 #202 — the equity-relative cap. It bounds ANY order that is not
        // the way OUT of a position, and only over-cap orders: it REJECTS, it
        // never truncates, because a truncated entry is a different trade than
        // the one the strategy asked for and the sizing decision belongs to
        // the sizing knobs (see `EngineConfig::size_pct`), not to a silent
        // clamp in the risk layer.
        //
        // `closes_exposure` is the same single judgment the kill switch uses,
        // and it is load-bearing here rather than decorative: a 10-share close
        // at 0.60 on a 4.8 USDC book is 6.00 USD, five times a 20% cap. If the
        // cap applied to it, the position could not be exited at all — the
        // #174 hole reopened through a different door.
        if !closes_exposure(req)
            && self.config.max_order_notional_pct > Decimal::ZERO
            && equity_usd > Decimal::ZERO
        {
            let cap = equity_usd * self.config.max_order_notional_pct / Decimal::ONE_HUNDRED;
            let notional = req.price * req.size;
            if notional > cap {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!(
                        "notional {notional} exceeds the {}% equity cap {cap} (equity {equity_usd}); \
                         size the order down or raise the cap — the cap never truncates",
                        self.config.max_order_notional_pct
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FillPolicy, Side};
    use rust_decimal_macros::dec;

    fn req(side: Side, price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "t".into(),
            condition_id: "c".into(),
            side,
            mode: FillPolicy::Taker,
            price,
            size,
            internal_key: "k".into(),
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn req_with_key(side: Side, key: &str) -> OrderRequest {
        OrderRequest {
            internal_key: key.into(),
            ..req(side, dec!(0.5), dec!(4))
        }
    }

    #[test]
    fn gates_notional_and_kill() {
        let mut g = RiskGate::new(RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        });
        // 0.5 * 4 = 2.0 within cap; 0.5 * 8 = 4.0 rejected.
        g.check(&req(Side::Buy, dec!(0.5), dec!(4))).unwrap();
        g.check(&req(Side::Buy, dec!(0.5), dec!(8))).unwrap_err();
        g.kill("manual");
        assert_eq!(
            g.check(&req(Side::Buy, dec!(0.5), dec!(1)))
                .unwrap_err()
                .code,
            CoreErrorCode::KillSwitchActive
        );
        g.resume();
        g.check(&req(Side::Buy, dec!(0.5), dec!(1))).unwrap();
    }

    /// P0 #174: the kill switch freezes entries, never the exit. A kill is
    /// raised exactly when the book is most dangerous, so a blocked closing
    /// SELL turns the stop-limited risk into a total one.
    #[test]
    fn kill_switch_freezes_entries_but_never_exits() {
        let mut g = RiskGate::new(RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        });
        g.kill("self-check failed");
        let buy = req(Side::Buy, dec!(0.5), dec!(1));
        assert_eq!(
            g.check(&buy).unwrap_err().code,
            CoreErrorCode::KillSwitchActive,
            "entries stay frozen"
        );
        // Every closing-intent prefix the placement path produces must pass.
        // The escalated variant is here because a maker→taker close leg keeps
        // the SAME key with `:escalated` appended (service.rs), so an exemption
        // that stopped at the first colon would re-lock the exit exactly when
        // the maker leg failed to fill.
        for key in [
            "exit:tok:StopLoss",
            "exit-strategy:tok:close",
            "flatten:hft-3",
            "exit-residual:tok",
            "exit:tok:StopLoss:escalated",
            "flatten:hft-3:escalated",
            "exit-residual:tok:escalated",
        ] {
            g.check(&req_with_key(Side::Sell, key))
                .unwrap_or_else(|e| panic!("{key} must pass the kill gate: {e}"));
        }
        // A SELL that does NOT name a close intent is not assumed to reduce
        // exposure, so it stays frozen with the entries.
        assert_eq!(
            g.check(&req_with_key(Side::Sell, "spread_arb:BTC"))
                .unwrap_err()
                .code,
            CoreErrorCode::KillSwitchActive
        );
        // The exemption is from the KILL gate only: the other invariants still
        // apply to a closing SELL (size must be positive, price in band).
        assert_eq!(
            g.check(&OrderRequest {
                internal_key: "flatten:hft-1".into(),
                size: Decimal::ZERO,
                ..req(Side::Sell, dec!(0.5), dec!(1))
            })
            .unwrap_err()
            .code,
            CoreErrorCode::RiskRejected
        );
        // ...and resuming restores entries.
        g.resume();
        g.check(&buy).unwrap();
    }

    /// P0 #202: the per-order notional bound written as a share of the account
    /// — and the one thing it must never do, block the way out of a position.
    #[test]
    fn equity_notional_cap_rejects_over_cap_orders_but_never_closes() {
        // A 4.8 USDC account with a 20% cap: 0.96 USD per order. The absolute
        // cap is deliberately wide (1000) so only the RELATIVE one can bite.
        let equity = dec!(4.8);
        let g = RiskGate::new(RiskConfig {
            max_order_notional: dec!(1000),
            max_order_notional_pct: dec!(20),
            ..Default::default()
        });
        // The measured live ticket — 10 shares at 0.40 = 4.00 = 83% of the
        // account — is refused. This is the order the 6.00 absolute cap waved
        // through (issue #202's premise).
        let err = g
            .check_with_equity(&req(Side::Buy, dec!(0.40), dec!(10)), equity)
            .unwrap_err();
        assert_eq!(err.code, CoreErrorCode::RiskRejected);
        assert!(
            err.message.contains("equity cap"),
            "the refusal must name the cap that bit: {}",
            err.message
        );
        // The largest whole ticket inside the cap passes: 2 shares = 0.80.
        g.check_with_equity(&req(Side::Buy, dec!(0.40), dec!(2)), equity)
            .unwrap();
        // "Any one order" means any: a SELL that does not name a close intent
        // is bounded too. Today's gate only capped BUYs; this is the wider
        // scope of the new cap, and it is why the exemption below matters.
        assert_eq!(
            g.check_with_equity(&req_with_key(Side::Sell, "spread_arb:BTC"), equity)
                .unwrap_err()
                .code,
            CoreErrorCode::RiskRejected
        );
        // Every close-intent spelling passes at a notional far over the cap
        // (10 shares at 0.60 = 6.00 = 6.25x). This is the escape hatch #174
        // depends on; a cap that re-locks it is #174 over again.
        for key in [
            "exit:tok:StopLoss",
            "exit-strategy:tok:close",
            "flatten:hft-3",
            "exit-residual:tok",
            "exit:tok:StopLoss:escalated",
            "flatten:hft-3:escalated",
        ] {
            let mut close = req_with_key(Side::Sell, key);
            close.price = dec!(0.60);
            close.size = dec!(10);
            g.check_with_equity(&close, equity)
                .unwrap_or_else(|e| panic!("{key} must pass the equity cap: {e}"));
        }
        // The exemption is exactly one gate wide: the other invariants still
        // apply to a closing SELL (here: size must be positive).
        assert_eq!(
            g.check_with_equity(
                &OrderRequest {
                    internal_key: "flatten:hft-1".into(),
                    size: Decimal::ZERO,
                    ..req(Side::Sell, dec!(0.5), dec!(1))
                },
                equity
            )
            .unwrap_err()
            .code,
            CoreErrorCode::RiskRejected
        );
    }

    /// Unconfigured (`pct = 0`) and equity-less calls must behave exactly as
    /// before: the relative cap exists only when someone asked for it, so a
    /// shipped deployment's order flow is unchanged by this PR.
    #[test]
    fn equity_notional_cap_is_inert_when_unconfigured_or_equityless() {
        let plain = RiskGate::new(RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        });
        // 6.00 over the absolute 3.00 cap → refused by the ABSOLUTE cap only.
        assert!(
            plain
                .check_with_equity(&req(Side::Buy, dec!(0.60), dec!(10)), dec!(4.8))
                .is_err()
        );
        // 2.00 under it → admitted, with or without an equity argument.
        plain
            .check_with_equity(&req(Side::Buy, dec!(0.5), dec!(4)), dec!(4.8))
            .unwrap();
        plain
            .check_with_equity(&req(Side::Buy, dec!(0.5), dec!(4)), Decimal::ZERO)
            .unwrap();
        // Relative cap armed, equity unknown: still inert (2.00 is 42% of 4.8,
        // and with the equity stated it IS refused — so the only difference
        // between these two lines is the argument).
        let armed = RiskGate::new(RiskConfig {
            max_order_notional: dec!(1000),
            max_order_notional_pct: dec!(20),
            ..Default::default()
        });
        armed
            .check_with_equity(&req(Side::Buy, dec!(0.5), dec!(4)), Decimal::ZERO)
            .unwrap();
        assert!(
            armed
                .check_with_equity(&req(Side::Buy, dec!(0.5), dec!(4)), dec!(4.8))
                .is_err()
        );
    }

    #[test]
    fn breaker_trips_on_consecutive_losses_and_resumes() {
        let mut b = LossBreaker::new(3, 300);
        assert!(!b.record(dec!(-1), 0));
        assert!(!b.record(dec!(-1), 1));
        assert!(b.record(dec!(-1), 2)); // 3rd loss trips
        assert!(b.is_halted(1000));
        // A win resets the streak but the halt persists until cooldown.
        assert!(!b.record(dec!(5), 2000));
        assert!(b.is_halted(2000));
        assert!(b.maybe_resume(300_100));
        assert!(!b.is_halted(300_100));
        assert_eq!(b.consecutive_losses(), 0);
    }

    /// KI-10 / D-18 option A: one leg's losing streak must freeze only that leg.
    #[test]
    fn a_per_strategy_breaker_does_not_freeze_other_strategies() {
        let mut bs = LossBreakers::new(3, 300);
        // Two losses are below the threshold; the third trips.
        assert!(!bs.record("trend_follow", dec!(-1), 0));
        assert!(!bs.record("trend_follow", dec!(-1), 1));
        assert!(bs.record("trend_follow", dec!(-1), 2));
        assert!(bs.is_halted("trend_follow", 1_000));
        assert!(
            !bs.is_halted("spread_arb", 1_000),
            "an unrelated strategy must stay free to enter"
        );
        assert!(!bs.is_halted("never_seen", 1_000));
        assert_eq!(bs.halted(1_000), vec!["trend_follow".to_string()]);

        // A win on the halted leg clears the streak but not the cooldown.
        assert!(!bs.record("trend_follow", dec!(5), 2_000));
        assert!(bs.is_halted("trend_follow", 2_000));
        // Other legs keep recording normally while the halt is active.
        assert!(!bs.record("spread_arb", dec!(-1), 2_000));
        assert_eq!(bs.consecutive_losses("spread_arb"), 1);

        assert_eq!(
            bs.maybe_resume_all(300_100),
            vec!["trend_follow".to_string()]
        );
        assert!(!bs.is_halted("trend_follow", 300_100));
        assert!(bs.halted(300_100).is_empty());
        // A second pass resumes nothing (the halt is already cleared).
        assert!(bs.maybe_resume_all(400_000).is_empty());
    }
}
