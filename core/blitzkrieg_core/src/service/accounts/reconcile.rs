//! Venue self-check and reconcile bookkeeping (#780).

use crate::ipc::schema::Event;
use crate::model::*;
use rust_decimal::Decimal;
use std::collections::HashSet;

use crate::service::{FREEZE_AUTO_LIFT_MIN_MS, PLACE_TIMEOUT_VERDICT_MS, SWEEP_FAILURE_FREEZE};

use super::Core;

impl Core {
    /// Trading-capability self-check result from the live venue bridge. A
    /// failed report freezes trading — the probe exercised the venue paths a
    /// live order shares (authenticated balance, reconciliation sweep), so
    /// failing them means order egress cannot work either.
    ///
    /// 2026-10-25 多场馆雪崩定谳：the freeze is VENUE-SCOPED — the failing
    /// probes are one venue's transport, so only the accounts bound to that
    /// venue stop entering. A global freeze here let polymarket's network
    /// blip veto a healthy polysimulator account's entries for the rest of
    /// the session (the two venues "took turns" being alive — each one's
    /// failure froze the other's survivors). The legacy global
    /// `freeze_trading` stays for causes with no venue dimension (audit
    /// halt, or a venue with no bound account); the `engine.stats.selfCheck`
    /// readout carries the last report regardless of which venue filed it.
    pub fn on_venue_self_check(
        &mut self,
        venue: &str,
        report: blitzkrieg_market_api::SelfCheckReport,
    ) {
        let ok = report.ok;
        let ts = report.ts_ms;
        for item in &report.items {
            eprintln!(
                "core: self-check [{}] {}: {} ({})",
                venue,
                item.name,
                if item.ok { "ok" } else { "FAIL" },
                item.detail
            );
        }
        self.last_self_check = Some(report);
        if !ok {
            // #184: name the failing probe(s) in the message, not just "failed".
            // The items are already in `engine.stats.selfCheck`, but an operator
            // reading the log needs the reason on the line itself.
            let failed = self
                .last_self_check
                .as_ref()
                .map(|r| {
                    r.items
                        .iter()
                        .filter(|i| !i.ok)
                        .map(|i| format!("{}: {}", i.name, i.detail))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let message = if failed.is_empty() {
                format!(
                    "trading self-check failed on {venue} (venue unreachable or credentials rejected)"
                )
            } else {
                format!(
                    "trading self-check failed on {venue}: {}",
                    failed.join("; ")
                )
            };
            tracing::error!(detail = %message, venue = %venue, "trading self-check failed");
            // Root cause into the one slot (code VENUE_ERROR, not the freeze's
            // own wording) — this path has no other carrier of its own.
            self.note_error(
                &crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message.clone(),
                ),
                ts,
            );
            // The freeze lands on the venue's bound accounts (multi-account
            // #532 routing): same contract as `freeze_account` — new entries
            // refused, closes keep flowing.
            for account in self.accounts.account_ids_bound_to_venue(venue) {
                self.freeze_account_auto(&account, message.clone(), ts);
            }
            // No bound account (unbound deployments, dry tests): keep the
            // legacy global freeze so the failure is never silently dropped.
            if !self.accounts.account_ids_bound_to_venue(venue).is_empty() {
                return;
            }
            self.freeze_trading(message, ts);
        }
    }

    /// E31-b: the executor's reconciliation sweep failed three times in a row.
    /// The sweep is the safety net that repairs missed fills, retires ghosts
    /// and cancels orphans; a blind sweep lets the book drift silently, so
    /// trading freezes until the operator or a restart re-probes the channel.
    ///
    /// VENUE-SCOPED (2026-10-25 多场馆雪崩定谳): `venue` names the reporting
    /// plugin; the freeze lands on the accounts bound to it. The per-venue
    /// streak lives in `consecutive_sweep_failures_by_venue` — a healthy
    /// sibling venue's sweeps keep resetting nothing here, and their accounts
    /// keep trading. The legacy global counter/`freeze_trading` path survives
    /// only for callers with no venue attribution (the in-kernel dry sweep).
    pub fn on_venue_reconcile_failed(
        &mut self,
        venue: &str,
        err: crate::model::CoreError,
        now_ms: i64,
    ) {
        let streak = self
            .consecutive_sweep_failures_by_venue
            .entry(venue.to_string())
            .or_default();
        *streak = streak.saturating_add(1);
        let streak = *streak;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            venue = %venue,
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            let accounts = self.accounts.account_ids_bound_to_venue(venue);
            if accounts.is_empty() {
                self.freeze_trading(
                    format!(
                        "reconciliation sweep failed {streak} times in a row on {venue}: {}",
                        err.message
                    ),
                    now_ms,
                );
                return;
            }
            for account in accounts {
                self.freeze_account_auto(
                    &account,
                    format!(
                        "reconciliation sweep failed {streak} times in a row on {venue}: {}",
                        err.message
                    ),
                    now_ms,
                );
            }
        }
    }

    /// Legacy global form kept for callers with no venue attribution. The
    /// in-kernel dry sweep and any pre-plugin path land here.
    pub fn on_reconcile_failed(&mut self, err: crate::model::CoreError, now_ms: i64) {
        self.consecutive_sweep_failures = self.consecutive_sweep_failures.saturating_add(1);
        let streak = self.consecutive_sweep_failures;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            self.freeze_trading(
                format!(
                    "reconciliation sweep failed {streak} times in a row: {}",
                    err.message
                ),
                now_ms,
            );
        }
    }

    /// Map a venue order id to a core order (for user-WS events). Returns the
    /// core id when known.
    pub fn core_id_for_venue(&self, venue_or_core: &str) -> Option<String> {
        self.ome
            .by_venue_or_id(venue_or_core)
            .map(|o| o.order_id.clone())
    }

    /// Live orders accepted locally but not yet submitted to the venue (no
    /// venue id bound). The live bridge submits these each tick.
    ///
    /// `place_uncertain` orders are excluded: their POST already timed out, so
    /// the venue's answer is unknown and a re-submit would be a SECOND real
    /// order at any venue without a client-id dedupe. The #462 retirement sweep
    /// (`resolve_place_timeouts`) still sees them and decides their fate.
    pub fn pending_unbound(&self) -> Vec<TrackedOrder> {
        self.ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none() && !o.place_uncertain)
            .cloned()
            .collect()
    }

    /// [`Self::pending_unbound`] filtered to ONE venue: the multi-plugin
    /// executor drain. The routing truth is the ORDER'S OWN execution venue
    /// (`execution_venue_for`: RFC-661 M1 — the placing account's binding
    /// resolves its OWN (venue, token) dual-key row first, so a shared
    /// token's two accounts each route to their own executor; the account
    /// dimension is the AMBIGUITY DISCRIMINATOR, not an override — a
    /// token-only read refuses on two candidate rows and could not pick)
    /// — an order whose execution venue
    /// resolves to nothing, or to a market with no venue label, is UNTAGGED
    /// and routes to the requested plugin only when it is the host's active
    /// plugin — the single-plugin behaviour, unchanged. With the lookup empty
    /// (engine down, replay corpora) every order is untagged, so each executor
    /// sees all of them exactly as the pre-routing bridge did.
    pub fn pending_unbound_for_venue(&self, venue: &str) -> Vec<TrackedOrder> {
        self.pending_unbound()
            .into_iter()
            .filter(|o| self.execution_venue_of(o.account_id.as_str(), &o.token_id) == venue)
            .collect()
    }

    /// The venue that should execute this order: the placing account's
    /// binding rides through [`Engine::execution_venue_for`] — RFC-661 M1:
    /// the (venue, token) dual-key table holds each venue's OWN row for a
    /// byte-identical token, so the binding's own row names the executor.
    /// The account parameter is the AMBIGUITY DISCRIMINATOR (a shared token
    /// carries one row per venue; a token-only read refuses on two
    /// candidates), not a mirror-era leftover — it stays. `""` when the
    /// engine is down or the token belongs to no market: UNTAGGED, never
    /// "venue empty-string" — callers route
    /// untagged work to the active plugin instead of guessing. Fallbacks to
    /// the plain table lookup when the engine is down (same empty answer,
    /// kept total for the host's drain seam, which cannot go through the
    /// engine handle).
    pub fn execution_venue_of(&self, account: &str, token_id: &str) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.execution_venue_for(account, token_id))
            .unwrap_or_default()
    }

    /// The venue a token belongs to under the live round list, `""` when the
    /// engine is down or the token belongs to no labelled market. Empty means
    /// UNTAGGED, never "venue empty-string": callers route untagged work to
    /// the active plugin instead of guessing.
    pub fn venue_of_token(&self, token_id: &str) -> String {
        self.engine
            .as_ref()
            .and_then(|e| e.venue_of(token_id))
            .unwrap_or_default()
    }

    /// Attach the venue id returned by a successful LIVE POST and mark it live.
    pub fn bind_venue(
        &mut self,
        core_id: &str,
        venue_order_id: String,
        now_ms: i64,
    ) -> CoreResult<()> {
        self.ome.bind_venue(core_id, venue_order_id, now_ms)?;
        self.ome.mark_live(core_id, now_ms)?;
        self.emit_order(core_id);
        Ok(())
    }

    /// Run a reconciliation sweep against a venue snapshot. Applies missed fills
    /// and repairs ghost/partial orders, emitting a report event.
    pub fn reconcile(
        &mut self,
        snap: crate::reconcile::VenueSnapshot,
    ) -> CoreResult<crate::reconcile::ReconcileReport> {
        self.reconcile_for_venue(snap, "")
    }

    /// [`Self::reconcile`] scoped to the REPORTING venue.
    ///
    /// With several plugins live every executor runs its own sweep carrying
    /// only ITS venue's open order ids. Unscoped, each sweep judged every
    /// venue's live orders and force-closed the other venues' resting orders as
    /// ghosts — they kept resting at their own venue, unmanaged, and the orphan
    /// sweep skipped them because `known_venue_ids` still listed them. `""`
    /// keeps the unscoped behaviour (single-venue hosts, in-process tests,
    /// replay).
    pub fn reconcile_for_venue(
        &mut self,
        snap: crate::reconcile::VenueSnapshot,
        venue: &str,
    ) -> CoreResult<crate::reconcile::ReconcileReport> {
        // Which orders actually rested at sweep time. The OME mutation below
        // stamps them terminal in one pass, so this is the only place the
        // `was_live` guard `cancel` uses can still be read (#776). Taken over
        // ALL live orders: the scoped sweep below only touches its own venue's
        // rows, so a sibling's id here is simply never consulted.
        let live_before: HashSet<OrderId> = self
            .ome
            .live_orders()
            .into_iter()
            .map(|o| o.order_id.clone())
            .collect();
        let report = if venue.is_empty() {
            crate::reconcile::reconcile(&mut self.ome, &snap)?
        } else {
            // Orders whose execution venue is a DIFFERENT one are not this
            // sweep's business. An order whose venue cannot be resolved today
            // (engine down, token delisted) stays in scope: no other sweep can
            // prove anything about it either.
            let skip: std::collections::HashSet<crate::model::OrderId> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| {
                    let own = self.execution_venue_of(o.account_id.as_str(), &o.token_id);
                    !own.is_empty() && own != venue
                })
                .map(|o| o.order_id.clone())
                .collect();
            crate::reconcile::reconcile_scoped(&mut self.ome, &snap, &skip)?
        };
        // The sweep channel just proved itself alive — a failed-sweep streak
        // only counts failures in a row (E31-b). The per-venue table clears
        // with it (the snapshot does not carry the reporting venue; one
        // successful sweep anywhere is evidence the transport fabric is
        // alive, and a fresh failure just rebuilds its own streak).
        self.consecutive_sweep_failures = 0;
        self.consecutive_sweep_failures_by_venue.clear();
        // 冻结自愈：a successful sweep proves the transport fabric alive, but
        // the snapshot names no venue — so every AUTO-frozen account gets the
        // same evidence, debounced by FREEZE_AUTO_LIFT_MIN_MS (a fresh freeze
        // rides out the flash outage that caused it before the lift fires).
        // Manual freezes are invisible to this loop by construction.
        for account in self.auto_frozen.keys().cloned().collect::<Vec<_>>() {
            if let Some(t) = self.auto_frozen.get(account.as_str())
                && snap.now_ms - t >= FREEZE_AUTO_LIFT_MIN_MS
            {
                self.auto_lift_if_venue_healthy(
                    account.as_str(),
                    "reconcile sweep succeeded",
                    snap.now_ms,
                );
            }
        }
        // Ledger + position effects for any gap fills the OME just applied.
        for gap in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::FilledGap { delta, .. } => Some(delta.clone()),
            _ => None,
        }) {
            self.apply_delta_effects(gap, snap.now_ms);
        }
        // Fills the sweep absorbed into an order that had already ended (#178):
        // the size is booked, the terminal status kept, and the operator told.
        for late in report.late_fills.clone() {
            self.emit_late_fill(&late, snap.now_ms);
        }
        // Trades on orders the OME never issued (manual closes made directly
        // on the venue): fold them into the position book so the position
        // cannot haunt the exit rules forever.
        self.apply_external_fills(&report.unknown_fills, snap.now_ms);
        // A terminal status the sweep just stamped (filled-without-gap,
        // vanished→cancelled, ghost→force-closed) must reach orders.jsonl the
        // same way every other status change does — otherwise a restart
        // re-adopts terminal orders as LIVE and force-closes them all over
        // again. emit_order already persists; these mutations bypass it.
        //
        // #776: a CANCELLED/FORCE-CLOSED order also owes the money bookkeeping
        // every other terminal transition runs — its BUY reservation released,
        // the strategy's virtual hold returned, the pair slot marked dead and
        // (for a hold-to-settlement strategy) the pending token lifted. The
        // sweep used to stamp the status alone, so a reconciled-away BUY kept
        // its reservation forever: the audit called it a stray hold, halted,
        // and every later entry was refused. One implementation
        // (`release_order_bookkeeping`), two callers — the explicit cancel and
        // this sweep. `was_live` comes from the pre-sweep snapshot, so the
        // operation is idempotent exactly like the cancel path.
        for action in &report.actions {
            match action {
                crate::reconcile::ReconcileAction::MarkedFilled { core_order_id } => {
                    self.persist_order(core_order_id);
                }
                crate::reconcile::ReconcileAction::MarkedCancelled { core_order_id }
                | crate::reconcile::ReconcileAction::MarkedForceClosed { core_order_id } => {
                    if let Some(order) = self.ome.get(core_order_id).cloned() {
                        let was_live = live_before.contains(order.order_id.as_str());
                        self.release_order_bookkeeping(&order, was_live, "reconcile");
                    }
                    self.persist_order(core_order_id);
                }
                crate::reconcile::ReconcileAction::FilledGap { .. } => {}
            }
        }
        // #462: retire place-timeout orders the venue never gave evidence for
        // (see `resolve_place_timeouts`).
        self.resolve_place_timeouts(&snap);
        if !report.actions.is_empty()
            || !report.suspect_ghost_ids.is_empty()
            || !report.unknown_fills.is_empty()
        {
            let filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. }))
                .count();
            let marked_filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedFilled { .. }))
                .count();
            let marked_cancelled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedCancelled { .. }))
                .count();
            let marked_force_closed = report
                .actions
                .iter()
                .filter(|a| {
                    matches!(
                        a,
                        crate::reconcile::ReconcileAction::MarkedForceClosed { .. }
                    )
                })
                .count();
            // #184: a sweep that had to REPAIR something is a drift report — the
            // local book disagreed with the venue and the safety net caught it.
            // The net working is good news; the divergence is not, and an
            // operator has to be able to see it in the run log without reading
            // the panel. Only repairs and ghost suspicions are errors: external
            // fills alone are a manual close made on the venue (informational).
            if !report.actions.is_empty() || !report.suspect_ghost_ids.is_empty() {
                tracing::error!(
                    filled_gaps = filled,
                    marked_filled,
                    marked_cancelled,
                    marked_force_closed,
                    ghosts = ?report.suspect_ghost_ids,
                    "reconciliation drift repaired: the venue disagreed with the core's book"
                );
            } else {
                tracing::info!(
                    external_fills = report.unknown_fills.len(),
                    "reconciliation: external (manual) fills folded into the position book"
                );
            }
            self.emit(Event::ReconcileReport {
                filled,
                marked_filled,
                marked_cancelled,
                marked_force_closed,
                ghost_ids: report.suspect_ghost_ids.clone(),
            });
        }
        Ok(report)
    }

    /// Fold external fills (venue trades on orders the OME does not track) into
    /// the position book. Only SELLs are applied: a manual close reduces the
    /// matching position by token id, settling the ledger for the cash that
    /// actually moved, and closes the position outright when it empties.
    /// Manual BUYS are skipped — cash truth comes from the periodic
    /// `venue_free_balance` realign, and inventing an unmanaged position here
    /// would fight the engine. Dedup is a persisted watermark: only fills
    /// strictly newer than the last applied one count, so a restart cannot
    /// re-apply what the sweep still reports.
    ///
    /// F2: the venue adapter only forwards legs it proved belong to our
    /// funder, and this loop stays conservative anyway — an unknown SELL is
    /// never booked beyond the inventory the token's position can actually
    /// attribute: size AND proceeds are truncated to what is held, so a
    /// mis-attributed over-sell cannot mint cash.
    pub(super) fn apply_external_fills(
        &mut self,
        fills: &[crate::reconcile::VenueTrade],
        now_ms: i64,
    ) {
        if fills.is_empty() {
            return;
        }
        let fresh: Vec<&crate::reconcile::VenueTrade> = fills
            .iter()
            .filter(|t| t.ts_ms > self.recon_watermark_ms)
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut applied = 0usize;
        for t in &fresh {
            if t.side != Side::Sell {
                continue;
            }
            let Some((position_id, held, position_account)) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == t.token_id)
                .map(|p| (p.id.clone(), p.shares, p.account_id.clone()))
            else {
                continue;
            };
            // F2: clamp the size to attributable inventory and price the cash
            // on the CLAMPED size — truncating the size while keeping the full
            // notional would still book the counterparty's money.
            let size = t.size.min(held);
            if size <= Decimal::ZERO {
                continue;
            }
            let notional = t.price * size;
            let fee_usd = if t.maker == Some(true) {
                Decimal::ZERO
            } else {
                // The fee follows the schedule in force, under the same
                // replay-only guard the fill choke point uses (#234, item 4).
                (self.live_taker_fee_pct(t.price) / Decimal::ONE_HUNDRED) * notional
            };
            // E28: a manual (external) close settles into the POSITION'S OWN
            // book — the proceeds belong to the account that paid for the
            // shares. An unknown account is logged and skipped (no implicit
            // book, no silent mint).
            if let Ok(l) = self.accounts.get_mut(&position_account) {
                l.settle_sell_fill(notional, fee_usd);
            } else {
                tracing::error!(
                    token = %t.token_id,
                    account = %position_account.as_str(),
                    "external sell for an unknown account dropped — proceeds not booked"
                );
                continue;
            }
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let remaining = self
                .positions
                .apply_exit_fill(&position_id, size, t.price, fee_usd, role, Decimal::ZERO)
                .unwrap_or(Decimal::ZERO);
            if remaining == Decimal::ZERO {
                // F10: a manual full close must hit the SAME close ledger the
                // engine's own fills do — TradeDb, strategy PnL, win/loss and
                // the consecutive-loss breaker. `close()` removes the position
                // from the book, so re-reporting the same trade cannot close
                // it twice: that removal IS the idempotence guard.
                if let Some(closed) = self.positions.close(
                    &position_id,
                    t.price,
                    ExitReason::Manual,
                    t.maker == Some(true),
                    now_ms,
                ) {
                    self.persist_positions();
                    self.on_position_closed(&closed, now_ms);
                }
                // Belt and braces: this path can close without the OME ever
                // issuing an order, and the pending-close reason must not
                // outlive the position it belonged to (issue #190) — the
                // reason is retired here even if no closed record came back.
                self.clear_exit_reason(&t.token_id);
            }
            self.persist_positions();
            applied += 1;
            tracing::info!(
                token = %t.token_id,
                requested_size = %t.size,
                applied_size = %size,
                price = %t.price,
                remaining = %remaining,
                "external close reconciled (manual venue sell folded into the position book)"
            );
        }
        // #462: fold unknown BUYs into the position book — a place-timeout
        // order the venue really filled arrives here (the OME never saw the
        // order id), and skipping it left real shares unmanaged while the
        // cash was already gone. Rebuilt through `apply_delta_effects`, the
        // ONE ledger+position+event path every other fill takes, so the
        // reconstructed position feeds exits/settlement exactly like an
        // engine-placed one. Metadata (strategy/condition/slot/direction)
        // comes from the engine's market registry for the token; a token no
        // live market names stays skipped (no inventable metadata), and the
        // default account takes the position (the ABI carries no account on
        // venue trades — the same default the SELL fold settles into).
        for t in fresh.iter().filter(|t| t.side == Side::Buy) {
            let Some(market) = self.engine.as_ref().and_then(|e| {
                e.scanner()
                    .markets()
                    .iter()
                    .find(|m| m.up_token_id == t.token_id || m.down_token_id == t.token_id)
            }) else {
                continue;
            };
            let direction = if market.up_token_id == t.token_id {
                "up"
            } else {
                "down"
            };
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let d = crate::ome::FillDelta {
                order_id: format!("recon:{}", t.venue_order_id),
                token_id: t.token_id.clone(),
                account_id: crate::model::default_account_id(),
                side: Side::Buy,
                delta: t.size,
                price: t.price,
                cumulative: t.size,
                strategy: "reconciled".to_string(),
                asset: market.asset.clone(),
                direction: direction.to_string(),
                condition_id: market.condition_id.clone(),
                round_slot: market.round_slot,
                mode: crate::model::FillPolicy::Taker,
                role,
                limit_price: t.price,
                trade_id: t.trade_id.clone(),
            };
            tracing::warn!(
                token = %t.token_id,
                venue_order = %t.venue_order_id,
                size = %t.size,
                price = %t.price,
                "external BUY reconciled — position rebuilt (a timed-out place the venue really filled, #462)"
            );
            self.apply_delta_effects(d, now_ms);
        }
        // Sells with no matching position are expected noise; one line per
        // sweep keeps the log honest. BUYs are folded above (#462).
        let unmatched = fresh.iter().filter(|t| t.side == Side::Sell).count() - applied;
        if unmatched > 0 {
            eprintln!("core: external fills not matched to a position: sells={unmatched}");
        }
        let new_mark = fresh
            .iter()
            .map(|t| t.ts_ms)
            .max()
            .unwrap_or(self.recon_watermark_ms);
        self.recon_watermark_ms = self.recon_watermark_ms.max(new_mark);
        if let Some(db) = self.position_db.as_ref() {
            db.save_watermark(self.recon_watermark_ms);
        }
    }

    /// Retire every order a place TIMEOUT left pending (#462) when the venue
    /// never gave evidence for it.
    ///
    /// `note_place_timeout` deliberately does NOT reject: the venue may have
    /// accepted the POST after the client gave up. But "pending forever"
    /// leaks the cash reservation and strands the intent, and the OME-side
    /// ghost sweep cannot judge these orders — they carry no venue id to
    /// match, so it skips them. The three endings all resolve without
    /// adoption:
    ///   really filled   → the unknown BUY fold in `apply_external_fills`
    ///                     books the position; this retirement then frees
    ///                     the now-stale reservation;
    ///   really resting  → the bridge's orphan sweep cancels the unknown
    ///                     venue id (exposure removed at the venue);
    ///   never received  → nothing at the venue; only this retirement
    ///                     closes the local intent.
    /// The budget clears the bridge's sweep cadence (5 s) plus the place
    /// timeout itself, so a POST that is merely slow is never raced, and
    /// the folded position occupies the per-asset slot in the window
    /// before retirement — a strategy re-attempt cannot double the entry.
    pub(super) fn resolve_place_timeouts(&mut self, snap: &crate::reconcile::VenueSnapshot) {
        for (id, submitted_at) in self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none() && o.status == OrderStatus::Pending)
            .map(|o| (o.order_id.clone(), o.submitted_at_ms))
            .collect::<Vec<_>>()
        {
            if snap.now_ms - submitted_at < PLACE_TIMEOUT_VERDICT_MS {
                continue;
            }
            if let Err(e) = self.cancel(&id, snap.now_ms) {
                tracing::warn!(order = %id, error = %e, "place-timeout retirement failed");
            } else {
                tracing::warn!(
                    order = %id,
                    waited_ms = snap.now_ms - submitted_at,
                    "place timed out and the venue never acknowledged it — order retired, reservation released (#462)"
                );
            }
        }
    }
}
