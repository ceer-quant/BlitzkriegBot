//! The four-gate pipeline (DEV_V0_3 §3.1/§3.3).
//!
//! Rules this file is held to (§3.3, a PR that breaks them is refused):
//! 1. PURE SHAPE — `process_intent` never places an order and never moves the
//!    ledger net; the `Approved`/`Modified` decision is handed to the existing
//!    `Core::place()` path by the caller. Gate 3 "reserves" through a
//!    probe-and-release so the EXISTING `Ledger::reserve` refusal logic is the
//!    one that answers, with a net-zero ledger effect.
//! 2. NO SHORT CIRCUIT — a Gate 1 rejection still produces a full trace list
//!    (short at Gate 1, but the audit always gets at least that one trace).
//! 3. NO DUPLICATED JUDGMENT — each gate CALLS the existing implementation
//!    (`OrderIntent::validate`, `RiskGate::check_with_equity`, the loss
//!    breakers, `Ledger::reserve`, `exit_policy::effective_stop_pct`); it
//!    never re-derives a threshold.
//! 4. CLOSE EXEMPTION — `is_close_intent` skips Gate 2 (existing semantics,
//!    "closing intents always pass") and Gate 4 (a close needs no survival
//!    binding). Not new behavior: the existing rule, written into the flow.

use super::{Decision, GateId, GateOutcome, GateTrace, LadderStep, PhysicsBinding, RejectReason};
use crate::exit_policy::{ExitConfig, effective_stop_pct};
use crate::ledger::Ledger;
use crate::model::{OrderRequest, Side};
use crate::risk::{LossBreakers, RiskGate, is_close_intent};
use blitzkrieg_market_api::{MarketType, OrderIntent, OrderKind, TimeInForce};
use rust_decimal::Decimal;
use std::time::Instant;

/// One normalized suggestion at the arbitration seam. It carries the kernel's
/// own [`OrderRequest`] unchanged — Gate 2 must judge THE SAME request the
/// existing path would judge, not a re-derived copy.
#[derive(Debug, Clone)]
pub struct StrategyIntent {
    /// The request exactly as the engine emitted it.
    pub request: OrderRequest,
}

impl StrategyIntent {
    pub fn from_request(request: OrderRequest) -> Self {
        Self { request }
    }

    /// The market-API shape Gate 1 validates. The mapping is mechanical:
    /// entries are resting limits on a prediction market.
    fn order_intent(&self) -> OrderIntent {
        OrderIntent {
            market: MarketType::Prediction,
            symbol: self.request.token_id.clone(),
            side: match self.request.side {
                Side::Buy => blitzkrieg_market_api::Side::Buy,
                Side::Sell => blitzkrieg_market_api::Side::Sell,
            },
            order_kind: OrderKind::Limit,
            price: Some(self.request.price),
            size: self.request.size,
            time_in_force: TimeInForce::Gtd,
            metadata: std::collections::HashMap::new(),
        }
    }

    fn is_close(&self) -> bool {
        is_close_intent(&self.request.internal_key)
    }
}

/// Everything the gates need, borrowed from the live kernel — never a copy of
/// a threshold. `ledger` is mutable ONLY for the Gate 3 probe (reserve +
/// release, net zero).
pub struct IntentCtx<'a> {
    /// Token ids tradable THIS round (up + down of every market). Entries on
    /// anything else are refused (Gate 1, "token 属于本轮").
    pub round_tokens: &'a [String],
    pub risk: &'a RiskGate,
    pub breaker: &'a LossBreakers,
    pub ledger: &'a mut Ledger,
    pub exit_cfg: &'a ExitConfig,
    pub time_left_sec: i64,
    pub now_ms: i64,
    /// The account's cash equity at judgment time (the #202 relative cap is a
    /// percentage of the LIVE balance, same rule as the submission path).
    pub equity: Decimal,
}

/// The pipeline's full product: the decision AND the traces the audit needs
/// (the audit record carries `gates` next to `decision`, §3.4), plus the wall
/// time the gates took.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub decision: Decision,
    pub gates: Vec<GateTrace>,
    pub latency_us: u64,
}

/// Run one suggestion through the four gates.
pub fn process_intent(intent: &StrategyIntent, ctx: &mut IntentCtx<'_>) -> Outcome {
    let started = Instant::now();
    let mut gates: Vec<GateTrace> = Vec::new();

    // ── Gate 1: legality (OrderIntent::validate + this-round token) ─────────
    if let Err(e) = intent.order_intent().validate() {
        let reason = match e.code {
            blitzkrieg_market_api::CoreErrorCode::InvalidSize => RejectReason::Malformed,
            _ if e.message.contains("price must be in") => RejectReason::OutOfPriceBand,
            _ => RejectReason::Malformed,
        };
        gates.push(GateTrace {
            gate: GateId::Legality,
            outcome: GateOutcome::Reject,
            detail: e.message.clone(),
        });
        return finish(
            Decision::Rejected {
                reason,
                gate: GateId::Legality,
                detail: e.message,
            },
            gates,
            started,
        );
    }
    // "token 属于本轮": an entry BUY on a token this round does not offer
    // cannot exist in the engine's output (candidates come from the round's
    // markets), so this re-check is latent at the live seam — it is here
    // because Gate 1's contract names it and the audit must be able to show
    // it. Closes are exempt: a held token may outlive its round.
    if intent.request.side == Side::Buy && !ctx.round_tokens.contains(&intent.request.token_id) {
        let detail = format!(
            "token {} is not part of round slot {}",
            intent.request.token_id, intent.request.round_slot
        );
        gates.push(GateTrace {
            gate: GateId::Legality,
            outcome: GateOutcome::Reject,
            detail: detail.clone(),
        });
        return finish(
            Decision::Rejected {
                reason: RejectReason::NotInRound,
                gate: GateId::Legality,
                detail,
            },
            gates,
            started,
        );
    }
    gates.push(GateTrace {
        gate: GateId::Legality,
        outcome: GateOutcome::Pass,
        detail: "OrderIntent::validate() ok".into(),
    });

    // ── Gate 2: system risk (existing RiskGate + the strategy's own breaker) ─
    if intent.is_close() {
        gates.push(GateTrace {
            gate: GateId::Risk,
            outcome: GateOutcome::Pass,
            detail: "close intent exempt — closing intents always pass (existing rule)".into(),
        });
    } else {
        if let Err(e) = ctx.risk.check_with_equity(&intent.request, ctx.equity) {
            let reason = match e.code {
                blitzkrieg_market_api::CoreErrorCode::KillSwitchActive => RejectReason::KillSwitch,
                _ => RejectReason::GlobalLimit,
            };
            gates.push(GateTrace {
                gate: GateId::Risk,
                outcome: GateOutcome::Reject,
                detail: e.message.clone(),
            });
            return finish(
                Decision::Rejected {
                    reason,
                    gate: GateId::Risk,
                    detail: e.message,
                },
                gates,
                started,
            );
        }
        // KI-10: only the ORDER'S OWN strategy's streak can block it — the
        // same breaker instance the submission path consults.
        if ctx.breaker.is_halted(&intent.request.strategy, ctx.now_ms) {
            let detail = format!(
                "loss breaker active until {} for strategy {}",
                ctx.breaker.halted_until_ms(&intent.request.strategy),
                intent.request.strategy
            );
            gates.push(GateTrace {
                gate: GateId::Risk,
                outcome: GateOutcome::Reject,
                detail: detail.clone(),
            });
            return finish(
                Decision::Rejected {
                    reason: RejectReason::LossBreaker,
                    gate: GateId::Risk,
                    detail,
                },
                gates,
                started,
            );
        }
        gates.push(GateTrace {
            gate: GateId::Risk,
            outcome: GateOutcome::Pass,
            detail: "RiskGate::check_with_equity ok, breaker clear".into(),
        });
    }

    // ── Gate 3: reservation probe (the existing Ledger::reserve refusal) ────
    if intent.request.side == Side::Buy {
        // PROBE-AND-RELEASE, not a real reservation: the submission path
        // reserves under the real order id after its own gates, and a second
        // reservation here would double-count the commitment. Reserving under
        // a probe id asks the EXISTING refusal logic the exact question the
        // submission path would ask ("does price*size fit what is available
        // right now?") and then releases, so the ledger is net-unchanged.
        let notional = intent.request.price * intent.request.size;
        const PROBE: &str = "__arbitration_probe__";
        if let Err(e) = ctx.ledger.reserve(PROBE, notional) {
            gates.push(GateTrace {
                gate: GateId::Reservation,
                outcome: GateOutcome::Reject,
                detail: e.message.clone(),
            });
            return finish(
                Decision::Rejected {
                    reason: RejectReason::InsufficientFunds,
                    gate: GateId::Reservation,
                    detail: e.message,
                },
                gates,
                started,
            );
        }
        ctx.ledger.release(PROBE);
        gates.push(GateTrace {
            gate: GateId::Reservation,
            outcome: GateOutcome::Pass,
            detail: format!("Ledger::reserve probe ok ({notional} USD)"),
        });
    } else {
        gates.push(GateTrace {
            gate: GateId::Reservation,
            outcome: GateOutcome::Pass,
            detail: "sells do not reserve (BUY-only reservation table)".into(),
        });
    }

    // ── Gate 4: survival binding (projection of the existing exit policy) ───
    let physics = if intent.is_close() {
        gates.push(GateTrace {
            gate: GateId::Physics,
            outcome: GateOutcome::Pass,
            detail: "close intent: no survival binding".into(),
        });
        PhysicsBinding {
            stop_price: Decimal::ZERO,
            force_exit_sec: 0,
            ladder: Vec::new(),
        }
    } else {
        let stop_pct =
            effective_stop_pct(ctx.exit_cfg.stop_loss_pct, ctx.time_left_sec, ctx.exit_cfg);
        let stop_price = intent.request.price * (Decimal::ONE - stop_pct / Decimal::from(100));
        // §4.3 projection: with no ladder configured, ONE step closing the
        // whole position at the configured take-profit — the same
        // "close it all at once" semantics the shipped kernel already has.
        let ladder = vec![LadderStep {
            at_pct: ctx.exit_cfg.take_profit_pct,
            close_ratio: Decimal::ONE,
            move_stop_to: None,
        }];
        gates.push(GateTrace {
            gate: GateId::Physics,
            outcome: GateOutcome::Pass,
            detail: format!(
                "projection: stop {stop_pct}% → {stop_price}, force-exit {}s, ladder[0] close 1.0 at {}%",
                ctx.exit_cfg.force_exit_sec, ctx.exit_cfg.take_profit_pct
            ),
        });
        PhysicsBinding {
            stop_price,
            force_exit_sec: ctx.exit_cfg.force_exit_sec,
            ladder,
        }
    };

    finish(
        Decision::Approved {
            request_id: intent.request.internal_key.clone(),
            shares: intent.request.size,
            price: intent.request.price,
            physics,
        },
        gates,
        started,
    )
}

fn finish(decision: Decision, gates: Vec<GateTrace>, started: Instant) -> Outcome {
    Outcome {
        decision,
        gates,
        latency_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FillPolicy, OrderRequest};
    use crate::service::CoreConfig;
    use rust_decimal_macros::dec;

    fn request(price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok-up".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::MakerThenTaker,
            price,
            size,
            internal_key: "probe-strategy:asset:up:1".into(),
            strategy: "probe-strategy".into(),
            asset: "asset".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn ctx<'a>(
        ledger: &'a mut Ledger,
        risk: &'a RiskGate,
        breaker: &'a LossBreakers,
        exit_cfg: &'a ExitConfig,
        tokens: &'a [String],
    ) -> IntentCtx<'a> {
        IntentCtx {
            round_tokens: tokens,
            risk,
            breaker,
            ledger,
            exit_cfg,
            time_left_sec: 600,
            now_ms: 1_000,
            equity: dec!(1000),
        }
    }

    fn setup() -> (Ledger, RiskGate, LossBreakers, ExitConfig, Vec<String>) {
        let cfg = CoreConfig::default();
        let mut ledger = Ledger::new();
        ledger.set_balance(dec!(1000));
        // A per-order cap high enough that Gate 3 (not Gate 2) is what answers
        // the oversized-entry test below.
        let mut risk_cfg = cfg.risk.clone();
        risk_cfg.max_order_notional = dec!(50000);
        (
            ledger,
            RiskGate::new(risk_cfg),
            LossBreakers::new(3, 60),
            cfg.positions.exit,
            vec!["tok-up".into(), "tok-down".into()],
        )
    }

    /// A clean suggestion clears all four gates and binds the existing exit
    /// policy's projection.
    #[test]
    fn clean_entry_clears_all_four_gates_and_binds_physics() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let out = process_intent(
            &StrategyIntent::from_request(request(dec!(0.40), dec!(10))),
            &mut c,
        );
        assert!(
            matches!(out.decision, Decision::Approved { .. }),
            "{:?}",
            out.decision
        );
        assert_eq!(out.gates.len(), 4, "{gates:?}", gates = out.gates);
        assert!(out.gates.iter().all(|g| g.outcome == GateOutcome::Pass));
        // The binding is the exit policy's own numbers, not a second opinion.
        let Decision::Approved {
            physics,
            shares,
            price,
            ..
        } = &out.decision
        else {
            panic!("expected Approved");
        };
        assert_eq!(*shares, dec!(10));
        assert_eq!(*price, dec!(0.40));
        assert_eq!(physics.force_exit_sec, exit_cfg.force_exit_sec);
        assert_eq!(physics.ladder.len(), 1);
        assert_eq!(physics.ladder[0].close_ratio, Decimal::ONE);
        let want_stop = dec!(0.40) * (Decimal::ONE - exit_cfg.stop_loss_pct / Decimal::from(100));
        assert_eq!(physics.stop_price, want_stop);
        // The probe released: the ledger is net-unchanged.
        assert_eq!(ledger.reserved(), Decimal::ZERO);
    }

    /// Gate 1: a price outside the prediction band is refused at LEGALITY and
    /// never reaches OME — and the trace still exists (no short circuit).
    #[test]
    fn band_violating_price_is_refused_at_legality_with_a_trace() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let out = process_intent(
            &StrategyIntent::from_request(request(dec!(1.5), dec!(10))),
            &mut c,
        );
        let Decision::Rejected { reason, gate, .. } = &out.decision else {
            panic!("expected Rejected, got {:?}", out.decision);
        };
        assert_eq!(*gate, GateId::Legality);
        assert_eq!(*reason, RejectReason::OutOfPriceBand);
        assert_eq!(out.gates.len(), 1);
        assert_eq!(out.gates[0].outcome, GateOutcome::Reject);
    }

    /// Gate 1: an entry on a token this round does not offer is refused.
    #[test]
    fn entry_on_a_foreign_token_is_refused_as_not_in_round() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let mut req = request(dec!(0.40), dec!(10));
        req.token_id = "tok-elsewhere".into();
        let out = process_intent(&StrategyIntent::from_request(req), &mut c);
        assert!(matches!(
            out.decision,
            Decision::Rejected {
                reason: RejectReason::NotInRound,
                gate: GateId::Legality,
                ..
            }
        ));
    }

    /// Gate 3: an entry larger than the whole available balance is refused by
    /// the EXISTING reserve refusal — and the probe left nothing behind.
    #[test]
    fn oversized_entry_is_refused_at_reservation_without_touching_the_ledger() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let out = process_intent(
            &StrategyIntent::from_request(request(dec!(0.40), dec!(100_000))),
            &mut c,
        );
        assert!(matches!(
            out.decision,
            Decision::Rejected {
                reason: RejectReason::InsufficientFunds,
                gate: GateId::Reservation,
                ..
            }
        ));
        assert_eq!(ledger.reserved(), Decimal::ZERO, "probe must release");
    }

    /// Gate 2: a close intent skips the risk gate and the binding (rule 4) but
    /// still leaves Gate 1 and Gate 3 traces — the audit shows the exemption.
    #[test]
    fn close_intent_is_exempt_at_risk_and_physics_but_still_traced() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let mut req = request(dec!(0.60), dec!(10));
        req.side = Side::Sell;
        req.internal_key = "exit:asset:1".into();
        let out = process_intent(&StrategyIntent::from_request(req), &mut c);
        assert!(matches!(out.decision, Decision::Approved { .. }));
        assert_eq!(out.gates.len(), 4);
        assert!(out.gates[1].detail.contains("exempt"));
        assert!(out.gates[3].detail.contains("no survival binding"));
    }

    /// Gate 2: a tripped breaker refuses with LossBreaker (KI-10: the order's
    /// OWN strategy's streak, same breaker instance the submission path uses).
    #[test]
    fn tripped_breaker_refuses_at_risk() {
        let (mut ledger, risk, mut breaker, exit_cfg, tokens) = setup();
        // Trip the strategy's breaker: max_consecutive_losses = 3 losses.
        for _ in 0..3 {
            breaker.record("probe-strategy", dec!(-1), 0);
        }
        assert!(breaker.is_halted("probe-strategy", 1_000));
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let out = process_intent(
            &StrategyIntent::from_request(request(dec!(0.40), dec!(10))),
            &mut c,
        );
        assert!(matches!(
            out.decision,
            Decision::Rejected {
                reason: RejectReason::LossBreaker,
                gate: GateId::Risk,
                ..
            }
        ));
    }

    /// Gate 2: the kill switch refuses NEW exposure with KillSwitch — the
    /// existing `check_with_equity` answer, mapped 1:1.
    #[test]
    fn kill_switch_refuses_entries_at_risk() {
        let (mut ledger, mut risk, breaker, exit_cfg, tokens) = setup();
        risk.kill("test");
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let out = process_intent(
            &StrategyIntent::from_request(request(dec!(0.40), dec!(10))),
            &mut c,
        );
        assert!(matches!(
            out.decision,
            Decision::Rejected {
                reason: RejectReason::KillSwitch,
                gate: GateId::Risk,
                ..
            }
        ));
    }

    /// P2 (§16.5): the arbitration overhead is MEASURED, not thresholded — the
    /// verdict criterion is P1 (the economic baseline holds), and the numbers
    /// land in `docs/perf/V0_3.md`. `--nocapture` prints p50/p99.
    #[test]
    fn arbitration_latency_p50_p99_measured() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let req = request(dec!(0.40), dec!(10));
        let intent = StrategyIntent::from_request(req);
        // Warm caches, then time the steady state.
        for _ in 0..200 {
            let _ = process_intent(&intent, &mut c);
        }
        let mut samples: Vec<u128> = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            let t0 = Instant::now();
            let _ = process_intent(&intent, &mut c);
            samples.push(t0.elapsed().as_micros());
        }
        samples.sort_unstable();
        let p = |q: f64| -> u128 {
            let idx = ((samples.len() as f64) * q).ceil() as usize;
            samples[(idx - 1).min(samples.len() - 1)]
        };
        println!(
            "process_intent latency over {} runs (debug build): p50={}us p99={}us max={}us",
            samples.len(),
            p(0.50),
            p(0.99),
            samples[samples.len() - 1]
        );
    }

    /// The rejection-storm acceptance (§14.1 E25): 4000 rejections in ONE
    /// second — every one is judged and traced, and the throttle folds the
    /// pushes to at most one per key per second. (The audit-side pairing of
    /// this lives in `audit::tests`.)
    #[test]
    fn four_thousand_rejections_per_second_are_all_judged_individually() {
        let (mut ledger, risk, breaker, exit_cfg, tokens) = setup();
        let mut c = ctx(&mut ledger, &risk, &breaker, &exit_cfg, &tokens);
        let req = request(dec!(1.5), dec!(10)); // always Gate-1-rejected
        let started = Instant::now();
        let mut rejected = 0usize;
        for _ in 0..4000 {
            let out = process_intent(&StrategyIntent::from_request(req.clone()), &mut c);
            if matches!(out.decision, Decision::Rejected { .. }) {
                rejected += 1;
            }
        }
        assert_eq!(rejected, 4000);
        assert!(
            started.elapsed().as_secs() < 2,
            "the gates must keep up with a storm"
        );
    }
}
