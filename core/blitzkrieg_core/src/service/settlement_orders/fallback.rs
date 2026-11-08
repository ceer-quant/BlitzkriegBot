//! The unverifiable-expiry fallback family (issue #991): sweep, policy-
//! switched cash-vs-receivable booking, receivable aging banner feed, and
//! local redemption at venue truth (resolution / reconcile / token priced).
//! Split verbatim from `settlement.rs` (pure code motion, no logic edits) —
//! the file-size ratchet (#780) does not rule content, it rules length.

use crate::model::*;
use crate::schema::Event;
use crate::service::config::UnverifiedExpiryPolicy;
use crate::settlement::{
    EXPIRY_FALLBACK_AFTER_MS, PRICE_SOURCE_CURRENT, PRICE_SOURCE_ENTRY_FALLBACK,
    UNVERIFIED_EXPIRY_EXIT_REASON,
};
use rust_decimal::Decimal;

use super::Core;

impl Core {
    /// whose expiry is long past AND whose verdict query has never been
    /// answered by the venue. The updown round list only carries future rounds,
    /// so an already-expired condition never appears in an answer: waiting is
    /// not patience, it is a ghost position forever (`positions.exit` bounces
    /// off the venue's "Insufficient position", and nothing else can clear it).
    ///
    /// The close is deliberately NOT a settlement:
    ///   * price = the position's last known mid; `0` (a dead feed) falls back
    ///     to the entry price, i.e. cost recovery — never an invented payout;
    ///   * NO redemption claim is booked (`SettlementBook::book` is not
    ///     called): with no verified verdict, redeeming a "winner" would be
    ///     redeeming an invention. The venue's own cash truth folds in through
    ///     the existing reconcile/realign sweep, which is exactly why this
    ///     price may stay conservative;
    ///   * the trade record's exit reason is `unverifiable_expiry`
    ///     ([`UNVERIFIED_EXPIRY_EXIT_REASON`]), so history can never be read
    ///     as a venue verdict.
    ///
    /// Idempotent by construction: the position leaves the open book on the
    /// first close, so the second pass finds nothing; the watch list forgets
    /// the market through [`Self::sync_settlement_watch`] once its last
    /// position is gone.
    pub(crate) fn run_expiry_fallback_sweep(&mut self, now_ms: i64) {
        // Only markets the venue has structurally never answered, past the
        // silence bound. `answers > 0` means the venue is ruling on this
        // condition honestly (even "not resolved yet") — keep waiting.
        let unanswered: std::collections::HashSet<String> = self
            .settlement
            .unanswered_since(EXPIRY_FALLBACK_AFTER_MS, now_ms)
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        if unanswered.is_empty() {
            return;
        }
        let targets: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| {
                unanswered.contains(&p.condition_id)
                    // Only past-expiry holdings qualify: a future expiry with an
                    // unanswered query is still (theoretically) settleable.
                    && p.expires_at_ms <= now_ms
            })
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        let closed_count = targets.len();
        for pos in targets {
            // Fallback price: the last known mid; a zeroed feed recovers cost
            // via the entry price. Both are bookkeeping values — the venue's
            // real cash position is what the reconcile sweep reports.
            // No floor: the price is booked as-is, so a dead feed (current_price
            // == 0) lands as a real -100% loss instead of being laundered into
            // a -50% fake loss. `price_source` (issue #1002) names which basis
            // the estimate used, so downstream surfaces can tell "the feed
            // said X" from "the feed was dead, entry price booked".
            let (price, price_source) = if pos.current_price > Decimal::ZERO {
                (pos.current_price, PRICE_SOURCE_CURRENT)
            } else {
                (pos.entry_price, PRICE_SOURCE_ENTRY_FALLBACK)
            };
            let closed = self.positions.close_priced(
                &pos.id,
                price,
                UNVERIFIED_EXPIRY_EXIT_REASON,
                true,
                now_ms,
                "fallback",
                price_source,
            );
            if let Some(closed) = closed {
                // The position left the open book, so the audit identity's
                // spent/received folds shrink by its flows. Something must
                // cover that walk — the policy decides WHAT (issue #991):
                //   * `cash` (default, #684 byte-identical): the fallback
                //     notional moves into spendable cash HERE, at the same
                //     bookkeeping price `close` used (fee-free maker close);
                //   * `receivable`: the notional becomes a claim in the
                //     settlement receivable pool instead — estimated money is
                //     NOT tradable money — and redeems into cash (with #998
                //     correction rows for the venue-vs-estimate delta) when
                //     venue truth arrives through the resolution/reconcile
                //     path.
                self.book_expiry_fallback(&closed, price, price_source, now_ms);
                // Trade record first, then the snapshot — the same order the
                // settlement path closes with (the audit reads the record).
                self.on_position_closed(&closed, now_ms);
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    account = %closed.account_id.as_str(),
                    shares = %closed.shares,
                    fallback_price = %price,
                    price_source = %price_source,
                    policy = %self.config.unverified_expiry_policy.as_str(),
                    "position closed at unverifiable expiry — the venue never answered this market's settlement query; \
                     booked per the unverifiable-expiry policy"
                );
            }
        }
        self.persist_positions();
        // A market whose LAST position left via the fallback: the watch would
        // only forget it on the next drain's `sync_settlement_watch` — forget
        // it NOW (the same thing `on_market_resolution` does), so the panel's
        // `trackedMarkets` reads 0 immediately and `unanswered_since` stops
        // naming a market we no longer hold.
        for condition_id in &unanswered {
            if self
                .positions
                .open_positions()
                .iter()
                .all(|p| p.condition_id != *condition_id)
            {
                self.settlement.forget_market(condition_id);
            }
        }
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::Internal,
            message: format!(
                "expiry fallback: closed {} position(s) on market(s) the venue never answered \
                 (unverifiable-expiry, booked per policy {})",
                closed_count,
                self.config.unverified_expiry_policy.as_str()
            ),
        });
    }

    /// Issue #991 boundary 1: the receivable-aging warn. When the OLDEST open
    /// fallback claim has waited longer than `BK_RECEIVABLE_MAX_AGE_MS`
    /// (default 48h), say so — once per episode: the warn fires on the
    /// crossing and stays quiet until the claim pool empties (redeemed) and
    /// refills past the bound again. The panel's amber banner is the
    /// CONTINUOUS face (`oldestFallbackClaimAgeMs` on every snapshot); this
    /// is the journal/log edge. Amber, never red: an aging receivable is
    /// attention, not a trading-down condition (red is reserved for the
    /// freeze and the failed self-check).
    pub(crate) fn check_receivable_aging(&mut self, now_ms: i64) {
        if self.config.unverified_expiry_policy != UnverifiedExpiryPolicy::Receivable {
            return;
        }
        let max_age = self.config.receivable_max_age_ms;
        if max_age <= 0 {
            return; // 0/negative = the check is off
        }
        let oldest = self.settlement.oldest_fallback_claim_age_ms(now_ms);
        match oldest {
            Some(age) if age > max_age && self.receivable_warned_age_ms == 0 => {
                self.receivable_warned_age_ms = age;
                let overdue = self
                    .settlement
                    .fallback_claims()
                    .filter(|c| now_ms - c.created_at_ms > max_age)
                    .count();
                let message = format!(
                    "receivable aging: {} fallback claim(s) overdue (oldest {age}ms > {max_age}ms) — \
                     the venue still has not answered; the receivable stays OUT of spendable cash \
                     until venue truth redeems it",
                    overdue.max(1)
                );
                tracing::warn!("{message}");
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::Internal,
                    message,
                });
            }
            None => {
                // The pool emptied (redeemed): re-arm the edge.
                self.receivable_warned_age_ms = 0;
            }
            _ => {}
        }
    }

    /// #684 shared ledger leg of the fallback close, under the issue-#991
    /// policy switch — THE one place both fallback call sites land. Without a
    /// compensating entry the position walks out of the open book and
    /// `trade_ledger_identity` drifts by exactly the closed notional; WHAT
    /// compensates is the policy:
    ///
    ///   * `cash` (default): `credit_expiry_fallback_cash` — the notional
    ///     (`price × dust_shares`; a maker close charges no exit fee) credits
    ///     spendable cash, kind `ExpiryFallback`, exactly as #684 shipped.
    ///     Byte-identical behaviour, no claim, no new state.
    ///   * `receivable`: `book_fallback_claim` — the notional becomes a
    ///     minimal `ExpiryFallbackClaim` in the settlement receivable pool
    ///     (the #461 merge-claim shape). Nothing credits cash: estimated
    ///     money must not be tradable money. The claim rides the SAME pool
    ///     (`receivable_usd_for`) the settled claims ride, so the audit's
    ///     cash identity, the panel and the day budget all see it as the
    ///     receivable it is. Redeems locally at venue truth
    ///     (`redeem_fallback_claims_for_condition` / `_for_token`).
    ///
    /// An unknown account cannot happen (the position was opened in a
    /// configured book), so a failure is logged loudly rather than folded
    /// into the active book's cash.
    pub(crate) fn book_expiry_fallback(
        &mut self,
        closed: &crate::position::ClosedPosition,
        price: Decimal,
        price_source: &str,
        now_ms: i64,
    ) {
        if self.config.unverified_expiry_policy == UnverifiedExpiryPolicy::Receivable {
            // Receivable model: book the claim from the position's OWN facts.
            // `dust_shares` is the un-sold remainder `close` priced at
            // `price`; `closed.net_pnl_usd` / `closed.exited_at_ms` are the
            // retract triple the redemption amends the trade record with.
            self.settlement.book_fallback_claim(
                &closed.id,
                &closed.condition_id,
                &closed.token_id,
                closed.account_id.as_str(),
                closed.dust_shares,
                price,
                price_source,
                closed.net_pnl_usd,
                closed.exited_at_ms,
                now_ms,
            );
            return;
        }
        self.credit_expiry_fallback_cash(closed, price);
    }

    /// #684 cash-model ledger leg (the default policy): credit what `close`
    /// priced the position's un-sold shares at (`price × dust_shares`; a
    /// maker close charges no exit fee) into the POSITION's own account book.
    /// The journal row's kind is `ExpiryFallback` — never a venue sell, never
    /// a redemption: the venue's truth stays the reconcile sweep's to report.
    /// An unknown account cannot happen (the position was opened in a
    /// configured book), so a failure is logged loudly rather than folded
    /// into the active book's cash.
    pub(crate) fn credit_expiry_fallback_cash(
        &mut self,
        closed: &crate::position::ClosedPosition,
        price: Decimal,
    ) {
        let amount = price * closed.dust_shares;
        if amount <= Decimal::ZERO {
            return;
        }
        let account = closed.account_id.clone();
        let position_id = closed.id.clone();
        if let Err(e) = self
            .accounts
            .get_mut(&account)
            .map(|l| l.credit_expiry_fallback(amount, &position_id))
        {
            tracing::error!(
                position = %position_id,
                account = %account.as_str(),
                error = %e,
                "expiry fallback credit FAILED — cash identity will drift by {amount}"
            );
        }
    }

    /// Issue #991 — redeem fallback receivable claims at VENUE TRUTH (the
    /// receivable policy's redemption leg). A claim redeems when the venue
    /// prices its token: a resolution for the condition (the venue's own
    /// verdict), or a venue SELL fill on the token from the reconcile sweep
    /// (venue cash moved at a real price). NOT a on-chain redeem — a fallback
    /// claim's collateral was never verified, so there is no CTF call and no
    /// tx hash; this is the accounting event that converts receivable into
    /// cash at the first venue-confirmed price.
    ///
    /// The booking is the #998 two-row shape (never double-counts):
    ///   * the claim's own `Redemption` row credits the BOOKED (estimated)
    ///     amount — exactly what the receivable carried;
    ///   * a correction `Redemption` row moves
    ///     `venue_truth − booked` when the two differ. A ZERO-DELTA
    ///     redemption books NO correction row (a correction of nothing is
    ///     not a fact — `apply` skips zero amounts, and the comment on
    ///     [`crate::ledger::Ledger::credit_redemption_with_correction`]
    ///     records the deliberate choice).
    ///
    /// The anchored trade-leg identity closes the loop: Δcash must equal
    /// Δrealized, so the recorded trade's net is AMENDED by the same delta
    /// (matched by the retract triple — position id, exit time, booked net —
    /// captured at booking time). The amend keeps `source`/`price_source`
    /// untouched: the row stays flagged as an estimated close whose price
    /// was never a venue verdict.
    ///
    /// `skip` names a position whose close record was amended by this same
    /// sweep already (one redemption per position per pass).
    pub(crate) fn redeem_fallback_claims_at(
        &mut self,
        truth: &dyn Fn(&str) -> Option<Decimal>,
        now_ms: i64,
    ) -> usize {
        if self.config.unverified_expiry_policy != UnverifiedExpiryPolicy::Receivable {
            return 0;
        }
        let position_ids: Vec<String> = self
            .settlement
            .fallback_claims()
            .map(|c| c.position_id.clone())
            .collect();
        let mut redeemed = 0usize;
        for position_id in position_ids {
            let Some(venue_price) = truth(&position_id) else {
                continue;
            };
            let Some((account_id, booked, booked_net, closed_at_ms, shares, fallback_price)) =
                self.settlement.redeem_fallback_claim(&position_id, now_ms)
            else {
                continue;
            };
            let venue_usd = venue_price * shares;
            let delta = venue_usd - booked;
            let a = AccountId::from(account_id.as_str());
            match self.accounts.get_mut(&a) {
                Ok(l) => l.credit_redemption_with_correction(booked, venue_usd, &position_id),
                Err(e) => {
                    // An unknown account cannot happen (the claim was booked
                    // from a configured book); if it ever does, the claim is
                    // already gone from the pool — surface it loudly rather
                    // than crediting a sibling book.
                    tracing::error!(
                        position = %position_id,
                        account = %account_id,
                        error = %e,
                        "fallback claim redemption names an unconfigured account — \
                         claim redeemed in the pool, cash NOT credited"
                    );
                }
            }
            // The trade record's net moves with the cash (Δrealized == Δcash),
            // matched on the retract triple captured at close.
            if delta != Decimal::ZERO
                && let Some(db) = self.trade_db.as_mut()
            {
                let amended =
                    db.amend_net_pnl(&position_id, closed_at_ms, booked_net, delta, now_ms);
                if !amended {
                    tracing::error!(
                        position = %position_id,
                        booked_net = %booked_net,
                        "fallback redemption could not amend the trade record — \
                         trade_ledger_identity will drift by {delta}"
                    );
                }
            }
            tracing::info!(
                position = %position_id,
                account = %account_id,
                booked_usd = %booked,
                venue_usd = %venue_usd,
                delta = %delta,
                fallback_price = %fallback_price,
                venue_price = %venue_price,
                "fallback receivable redeemed at venue truth (zero-delta redemptions book no correction row)"
            );
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "fallback receivable redeemed: position {position_id} booked {booked} USDC, \
                     venue truth {venue_usd} USDC (Δ{delta})",
                ),
            });
            redeemed += 1;
        }
        redeemed
    }

    /// Issue #991: redeem every fallback claim the RESOLUTION prices — the
    /// venue's own verdict for the condition, the strongest truth there is.
    /// A resolution that does not cover the claim's token leaves the claim
    /// open (the same posture an unpriced open position takes).
    pub(crate) fn redeem_fallback_claims_for_resolution(
        &mut self,
        resolution: &blitzkrieg_market_api::MarketResolution,
        now_ms: i64,
    ) -> usize {
        // Truth map first (an &self read), redemption second (&mut self) —
        // the same two-phase shape the settlement booking uses.
        let truth: std::collections::HashMap<String, Decimal> = self
            .settlement
            .fallback_claims()
            .filter(|c| c.condition_id == resolution.condition_id)
            .filter_map(|c| {
                resolution
                    .payout_per_share(&c.token_id)
                    .map(|p| (c.position_id.clone(), p))
            })
            .collect();
        if truth.is_empty() {
            return 0;
        }
        self.redeem_fallback_claims_at(&move |position_id| truth.get(position_id).copied(), now_ms)
    }
}
