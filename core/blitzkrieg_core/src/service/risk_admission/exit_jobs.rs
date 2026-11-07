//! The strategy-exit submission pipeline, split from `exits.rs` verbatim
//! (issue #963: the #780 file-size ratchet — pure code motion, no logic
//! edits). The intent half resolves `StrategyExitIntent`s into [`ExitJob`]s
//! and the submission half places them; `run_exit_checks` (which stays in
//! `exits.rs`, adding the policy-ladder and residual halves) and the engine
//! cycle's same-cycle submission (`submit_strategy_exits_now`, the #963 exit
//! priority fix) share BOTH halves here.

use super::exits::{
    exit_reason_from_strategy_tag, position_pnl_pct, reason_is_profit_side,
    strategy_signal_sell_bid,
};
use super::*;

/// A unit of closing work resolved against a concrete open position.
///
/// #963: hoisted from `run_exit_checks`'s local scope so the strategy-signal
/// half of the exit pipeline (`exit_jobs_from_strategy_intents` +
/// `submit_exit_jobs`) can also serve the engine cycle's same-cycle intent
/// submission — an exit signal born in `engine_evaluate` must not wait behind
/// the entries that same cycle places.
#[derive(Clone)]
pub(crate) struct ExitJob {
    pub(crate) position_id: String,
    pub(crate) token: String,
    pub(crate) condition_id: String,
    pub(crate) price: Decimal,
    pub(crate) reason: ExitReason,
    pub(crate) use_maker: bool,
    /// #915: the strategy's own maker-TP limit (validated against the
    /// live book in the submission loop before it is used). `None` =
    /// the kernel's own maker pricing (`maker_exit_price`).
    pub(crate) exit_price: Option<Decimal>,
    pub(crate) internal_key: String,
    pub(crate) strategy: String,
    pub(crate) asset: String,
    pub(crate) direction: String,
    /// E28 (§9.2): the account the closing SELL spends from — the
    /// position's own book, so an exit can never debit another account.
    pub(crate) account_id: AccountId,
    /// Issue #464: shares to sell (`None` = the whole sellable
    /// position, every path's historical behavior). A ladder step's
    /// tranche rides here from the `ExitRequest`.
    pub(crate) close_shares: Option<Decimal>,
}

impl Core {
    /// #963: submit strategy-signal close intents IMMEDIATELY, before this
    /// cycle's entries are placed. The live engine cycle drains exit intents
    /// from the strategies in the same call that places their entries; the
    /// deferred path (extend `strategy_exits`, consume next tick's
    /// `run_exit_checks`) let the entry win the same-cycle race every time —
    /// the exit signal waited a full maintenance interval behind the very
    /// orders it races for liquidity. Replays keep the deferred path (the
    /// #390 scheduling shape is output-pinned); merge intents stay deferred
    /// too (a pair collection is settlement work, not a liquidity race).
    pub(crate) fn submit_strategy_exits_now(
        &mut self,
        intents: Vec<crate::strategies::StrategyExitIntent>,
        now_ms: i64,
    ) {
        let (merge_intents, sell_intents): (Vec<_>, Vec<_>) =
            intents.into_iter().partition(|i| i.reason == "merge");
        self.strategy_exits.extend(merge_intents);
        let mut has_job: HashSet<String> = HashSet::new();
        let jobs = self.exit_jobs_from_strategy_intents(sell_intents, &mut has_job, now_ms);
        self.submit_exit_jobs(jobs, now_ms);
    }

    /// Resolve strategy-signal close intents into [`ExitJob`]s: the intent half
    /// of `run_exit_checks` (position resolution, the fresh-bid pricing gate,
    /// the maker-first advisory gates, reason mapping). Verbatim from the loop
    /// it replaces (pure code motion, #963).
    pub(crate) fn exit_jobs_from_strategy_intents(
        &mut self,
        intents: Vec<crate::strategies::StrategyExitIntent>,
        has_job: &mut HashSet<String>,
        now_ms: i64,
    ) -> Vec<ExitJob> {
        let mut jobs: Vec<ExitJob> = Vec::new();
        // Strategy-signal exits: resolve the token to a live position and price
        // off its latest valuation, exactly like a manual flatten. The intent's
        // OWN account is the first key (账户↔venue 解耦: two scopes can hold
        // the same token; the close must land on the intending scope's row),
        // and the job's internal key carries the account segment so two
        // accounts' exit ladders never collide in the place-cooldown/dedup
        // map. A legacy intent (no account — the pre-fanout shape) resolves
        // to `default`, the same row it always hit.
        for intent in intents {
            // #915: the strategy MAY ride its own maker-TP limit
            // (`exit_price`) on the intent. It is honoured ONLY when the same
            // maker-first gate stack accepts the close as a maker — a price
            // advisory can never convert a taker exit into a maker one.
            let strategy_exit_price = intent
                .exit_price
                .as_deref()
                .and_then(|s| Decimal::from_str_exact(s).ok());
            let intent_account = AccountId::from(intent.account_id.as_str());
            let Some(pos) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id && p.account_id == intent_account)
                .cloned()
            else {
                continue;
            };
            if !has_job.insert(pos.id.clone()) {
                continue; // an automated/policy exit already closes it this cycle
            }
            // Best bid only. A missing or stale bid must not fall through to
            // the mid or to `current_price` (a display mark). The strategy
            // already judged the exit off this same bid; selling a different
            // number would book a profit nobody was offering.
            let Some(price) = strategy_signal_sell_bid(
                self.books.get(&pos.token_id),
                now_ms,
                self.config.positions.exit.max_book_age_sec,
            ) else {
                tracing::info!(
                    token = %pos.token_id,
                    strategy = %pos.strategy,
                    "strategy exit withheld: no fresh best bid (will not sell the mid)"
                );
                continue;
            };
            let tag: String = intent
                .reason
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(32)
                .collect();
            // #935 fix 1: the strategy's own reason is what the ledger records
            // when this close fills. Only tags that map to a real `ExitReason`
            // variant pass through; an unknown tag keeps the historic blanket
            // `StrategySignal` stamp (old files are never rewritten either
            // way, so history stays stable).
            let reason = exit_reason_from_strategy_tag(&intent.reason);
            // #935 fix 2: a TP-class close may rest as a maker (the advisory
            // rides the intent; the gate + the driver's own `use_maker` have
            // to agree). Stops and time outs never convert — #915's
            // counterfactual showed a resting stop does not protect a
            // crashing leg (0/2), so `use_maker` on those reasons is ignored
            // no matter what the strategy declared.
            // #939: the depth-loss half. `reason_is_profit_side` passes a
            // `trailing_stop` tag from the ROUND side of the ledger, but the
            // trailing math itself anchors nothing at entry (`gave_back`
            // constrains peak−cur only; manifest knobs like arm=0/give=500
            // fire it below the entry too) — gate on the position's own
            // holders' PnL mark instead: a close the kernel is ALREADY below
            // its entry on is a loss to be escaped, not a profit to bank.
            // Resting it above the bid would repeat #915's "stop resting,
            // 0/2 filled" (the maker-first maker never crosses a falling
            // touch until `maker_timeout_ms` escalates it), so when the
            // position is underwater the advisory degrades to the taker
            // sweep right here.
            let pnl_pct = position_pnl_pct(&pos);
            let use_maker = intent.use_maker
            && reason_is_profit_side(reason)
            && self.config.positions.exit.strategy_exit_maker_first
            // #939 labor-core kernel PnL guard: proceeds at-or-above entry only.
            && pnl_pct >= Decimal::ZERO;
            if intent.use_maker && !use_maker {
                tracing::debug!(
                    token = %pos.token_id,
                    strategy = %pos.strategy,
                    reason = %intent.reason,
                    pnl_pct = %pnl_pct,
                    "maker-first exit advisory not applied (reason class, kernel gate, or underwater)"
                );
            }
            // #915: the intent's own price is candidate material ONLY in the
            // maker path. Taker closes always price off the fresh best bid
            // (the #180 rule — the kernel sells at what a buyer offers, never
            // at a number a strategy quoted); a `Some` price on a taker
            // conversion is advisory-only and silently unused, matching how
            // `use_maker` degrades.
            let exit_price = if use_maker { strategy_exit_price } else { None };
            jobs.push(ExitJob {
                position_id: pos.id.clone(),
                token: pos.token_id.clone(),
                condition_id: pos.condition_id.clone(),
                price,
                reason,
                use_maker,
                // #915: the strategy's own post-only limit rides the job —
                // the submission loop validates it against the live book
                // before using it.
                exit_price,
                internal_key: exit_intent_key("exit-strategy", &pos, &tag),
                strategy: pos.strategy.clone(),
                asset: pos.asset.clone(),
                direction: pos.direction.as_str().to_string(),
                // E28 (§9.2): the strategy-signal exit spends from the
                // position's own account.
                account_id: pos.account_id.clone(),
                close_shares: None,
            });
        }

        jobs
    }

    /// Submit resolved [`ExitJob`]s: the submission half of `run_exit_checks`
    /// (live-sell dedup, reject backoff, sizing, maker/taker pricing, place).
    /// Verbatim from the loop it replaces (pure code motion, #963).
    pub(crate) fn submit_exit_jobs(&mut self, jobs: Vec<ExitJob>, now_ms: i64) {
        for job in jobs {
            let already_live = self
                .ome
                .live_for(&job.token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live());
            if already_live {
                continue;
            }
            // The venue already refused this exit a moment ago: let the
            // backoff run instead of re-POSTing every tick (the 380+-rejection
            // storm this guards against). Silent skip — the rejection was
            // already accounted for when it happened.
            if self.placement_blocked(&job.internal_key, now_ms) {
                continue;
            }
            // #464: a ladder tranche sells exactly its floor-to-grid size
            // (never more than the position still holds); `None` sells the
            // whole sellable position — every no-ladder path, unchanged.
            let size = if let Some(tranche) = job.close_shares {
                let Some(whole) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                tranche.min(whole)
            } else {
                let Some(size) = self.positions.sell_shares(&job.position_id) else {
                    continue;
                };
                size
            };
            // #963: a maker-first exit rests as `MakerThenTaker` under the
            // exit's OWN escalation clock (`exit_maker_timeout_ms`, shipped
            // 250 ms) — it used to rest as a pure `Maker` and NEVER escalate,
            // while entries waited at most the 5 s entry default. The exit
            // the operator most wants filled had no deadline at all. A
            // negative timeout keeps the pure-maker posture (the operator's
            // off switch); `0` falls back to the entry default, the pre-#963
            // shape. Stops and every other taker sweep are unchanged.
            let mode = if job.use_maker {
                FillPolicy::MakerThenTaker
            } else {
                FillPolicy::Taker
            };
            // A FOK sell must fill in FULL at or above its limit; priced at
            // the touch it dies on the first thin tick (thin books race the
            // quote). Selling LOWER only widens the marketable range — the
            // venue still fills at the bid or better, so the slip is a floor
            // for where we agree to sell, not a price we pay.
            //
            // #496: a resting (maker) sell priced AT the bid is a crossing
            // post-only order — the venue refuses it every time, so a live TP
            // exit could never rest (the rejection storm #163 only backs off;
            // dry's close-intent exemption masked it). The resting sell goes
            // one tick ABOVE the bid, clamped to stay strictly inside the
            // ask (a price at/above the ask would cross the other side and
            // be refused too). No bid or no book: there is nothing to rest
            // against, so the job's judged price stands and the venue's own
            // refusal (or the dry book aging) reports it.
            //
            // #915: a strategy-quoted maker limit (`exit_price`) is honoured
            // only when it is post-only-safe against the LIVE book — strictly
            // above the best bid (resting, never crossing) and at or below
            // the best ask (never crossing the other side). Anything else
            // (no book, no bid,<=bid,>=ask) falls back to the kernel's own
            // maker pricing: the advisory can choose where to rest among the
            // legal grid points, it cannot widen what the kernel pays for.
            let price = if mode == FillPolicy::Taker {
                marketable_exit_price(job.price, self.config.exit_taker_slip_ticks)
            } else {
                match job.exit_price {
                    Some(quoted) => {
                        // #915: live book only. A quote is post-only-safe
                        // when it rests STRICTLY above the best bid (never
                        // crossing) at or under a LIVE best ask (never
                        // crossing the other side) — otherwise the kernel's
                        // own maker pricing takes over. The 0.99 / 0.01
                        // clamp is the same cell the path's other legs use.
                        let safe = self
                            .books
                            .get(&job.token)
                            .map(|b| {
                                b.best_bid()
                                    .zip(b.best_ask().filter(|a| *a > Decimal::ZERO))
                                    .is_some_and(|(bid, ask)| quoted > bid && quoted <= ask)
                            })
                            .unwrap_or(false);
                        let price = if safe {
                            quoted
                        } else {
                            tracing::debug!(
                                token = %job.token,
                                quoted = %quoted,
                                "strategy maker-TP limit not post-only-safe — kernel pricing takes over"
                            );
                            self.maker_exit_price(&job.token, job.price)
                        };
                        price.min(Decimal::new(99, 2)).max(Decimal::new(1, 2))
                    }
                    None => self.maker_exit_price(&job.token, job.price),
                }
            };
            let order = OrderRequest {
                token_id: job.token.clone(),
                condition_id: job.condition_id,
                side: Side::Sell,
                mode,
                price,
                size,
                internal_key: job.internal_key,
                strategy: job.strategy,
                asset: job.asset,
                direction: job.direction,
                round_slot: 0,
                // E28 (§9.2): the closing SELL spends from the position's own
                // account (carried on the job from the position).
                account_id: job.account_id,
            };
            // Record the intended exit reason so a full sell fill closes with it
            // (bounded and cleaned up by the close: issue #190).
            self.note_exit_reason(&job.token, job.reason);
            // #963: the exit's own maker→taker clock rides here — a taker
            // job ignores it (nothing to escalate), a maker job arms its
            // escalation at `exit_maker_timeout_ms` instead of resting
            // forever.
            if let Err(e) = self.place(
                order,
                self.config.positions.exit.exit_maker_timeout_ms,
                now_ms,
            ) {
                self.emit_error(e);
            }
        }
    }
}
