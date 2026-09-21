//! Live bridge — connects a market plugin's `OrderExecutor` to the host.
//!
//! Runs only in live mode. It submits locally-accepted orders, drains the venue's
//! user-WS events into the host, and runs the periodic REST reconciliation sweep.
//! Everything goes through the `MarketHost` seam, so this module needs no core
//! types and lives entirely inside the Polymarket extension.
//!
//! If credentials are absent it stays inert, so a live binary without keys still
//! serves the socket.

use crate::feed::now_ms;
use crate::redeem::{self, RedeemConfig, RedeemOutcome};
use crate::venue::{VenueEvent, now_epoch_ms, spawn_from_env};
use alloy::primitives::Address;
use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, MarketHost, ReconcileSnapshot, RedemptionFailure, RedemptionRequest,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Minimum spacing between failure-triggered capability probes, so a
/// persistently broken venue cannot turn the probe itself into a storm.
const PROBE_COOLDOWN_MS: i64 = 60_000;

/// A venue order the bridge itself placed is protected from the periodic
/// orphan sweep for this long: the core's `known_venue_order_ids` may lag a
/// freshly accepted order by one bridge turn, and sweeping our own resting
/// entry would fight the engine.
const ORPHAN_GRACE_MS: i64 = 20_000;

/// Consecutive reconciliation sweeps that may fail before the bridge reports
/// the sweep channel itself as broken: the host freezes trading on it (E31-b).
const SWEEP_FAILURE_FREEZE: u32 = 3;

/// Redemptions signed and broadcast at once. A redemption is a real
/// transaction: two sends from one signer race the same nonce, so the bridge
/// keeps the concurrency at the level one wallet can serialize.
const REDEEM_MAX_IN_FLIGHT: usize = 2;

/// Spawn the live bridge if credentials are present. `markets` only gates the
/// spawn (live with no markets = REST-only reconciliation); the user-WS
/// subscribes to the account-wide user stream so fills for later rounds are
/// never missed.
pub async fn spawn_if_configured(
    host: Arc<dyn MarketHost>,
    markets: Vec<String>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    if std::env::var("POLYMARKET_PRIVATE_KEY").is_err()
        || std::env::var("POLYMARKET_FUNDER_ADDRESS").is_err()
    {
        eprintln!(
            "polymarket-extension: live mode but POLYMARKET_* credentials absent — venue bridge disabled"
        );
        return Ok(None);
    }

    let (evt_tx, mut evt_rx) = mpsc::channel::<VenueEvent>(256);
    let venue = spawn_from_env(markets, evt_tx).await?;
    let (signer, funder) = (venue.signer.clone(), venue.funder.clone());

    // Redemption config, read once: a settled claim is redeemed on-chain by the
    // bridge itself when an endpoint is available, and reported as MANUAL
    // (loudly, and only once per claim) when it is not — never left silently
    // un-redeemed while the ledger waits for cash.
    let redeem_cfg = match (Address::from_str(&signer), Address::from_str(&funder)) {
        (Ok(s), Ok(h)) => RedeemConfig::from_env(s, h),
        (s, h) => Err(RedemptionFailure {
            message: format!("signer/funder address unreadable: {s:?} / {h:?}"),
            manual: true,
        }),
    };
    match &redeem_cfg {
        // Never echo the endpoint: it is operator config and may carry a key.
        Ok(c) => eprintln!(
            "polymarket-extension: on-chain redemption enabled (endpoint set, holder {})",
            c.holder
        ),
        Err(f) => eprintln!(
            "polymarket-extension: on-chain redemption NOT configured ({}); settled claims will \
             be reported as requiring manual redemption",
            f.message
        ),
    }

    // Seed the ledger from the venue so the reserve gate reflects real cash.
    match venue.balance().await {
        Ok(b) => {
            host.seed_balance(b).await;
            eprintln!(
                "polymarket-extension: live bridge ready signer={signer} funder={funder} balance={b}"
            );
        }
        Err(e) => eprintln!("polymarket-extension: live balance fetch failed: {e}"),
    }

    // Startup capability self-check: if the venue paths trading needs are
    // broken (bad credentials, geoblock, venue down) the host freezes trading
    // BEFORE any order can go out — the same freeze an in-run probe triggers.
    match venue.self_check().await {
        Ok(report) => {
            if !report.ok {
                eprintln!("polymarket-extension: startup self-check FAILED");
            }
            host.on_self_check(report).await;
        }
        Err(e) => eprintln!("polymarket-extension: startup self-check failed to run: {e}"),
    }

    // Startup orphan sweep: any order the venue still holds that the core does
    // NOT know about is an orphan (left by a crash or a previous process). Cancel
    // it before trading resumes so the bot can never walk away from a resting
    // order it does not track. Runs exactly once, at start.
    match venue.snapshot().await {
        Ok((venue_open, _trades)) => {
            let known: std::collections::HashSet<String> =
                host.known_venue_order_ids().await.into_iter().collect();
            let mut cancelled = 0usize;
            for id in venue_open {
                if known.contains(&id) {
                    continue; // ours and tracked — leave it resting
                }
                match venue.cancel(id.clone()).await {
                    Ok(()) => {
                        cancelled += 1;
                        host.note_orphan_cancelled(&id).await;
                        eprintln!("polymarket-extension: cancelled orphan order {id}");
                    }
                    Err(e) => {
                        eprintln!("polymarket-extension: failed to cancel orphan {id}: {e}");
                        host.report_error(CoreError::new(
                            CoreErrorCode::VenueError,
                            format!("orphan cancel failed for {id}: {e}"),
                        ))
                        .await;
                    }
                }
            }
            if cancelled > 0 {
                eprintln!(
                    "polymarket-extension: startup sweep cancelled {cancelled} orphan order(s)"
                );
            }
        }
        Err(e) => eprintln!(
            "polymarket-extension: startup orphan sweep skipped (snapshot failed: {e}; raw={:?})",
            e.raw
        ),
    }

    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let mut since_reconcile = 0u32;
        let mut place_failures: u32 = 0;
        let mut sweep_failures: u32 = 0;
        let mut next_probe_ok_ms: i64 = 0;
        // Venue ids the bridge placed recently (orphan-sweep grace window).
        let mut recent_placements: Vec<(String, i64)> = Vec::new();
        // Redemptions in flight (each one a signed transaction) and the claims
        // that could not get a slot yet.
        let mut redeems: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let mut redeem_queue: std::collections::VecDeque<RedemptionRequest> =
            std::collections::VecDeque::new();
        loop {
            tick.tick().await;

            // 1) Submit locally-accepted orders not yet on the venue. A venue
            // rejection rides to the host WITH its error text (cooldowns,
            // panel visibility, consecutive-failure freeze all key off it).
            for order in host.take_pending_orders().await {
                let core_order_id = order.core_order_id.clone();
                match venue.place(order).await {
                    Ok(p) => {
                        place_failures = 0;
                        recent_placements.push((p.venue_order_id.clone(), now_epoch_ms()));
                        host.on_order_accepted(&core_order_id, &p.venue_order_id)
                            .await
                    }
                    Err(e) => {
                        place_failures = place_failures.saturating_add(1);
                        host.on_order_rejected(&core_order_id, Some(e.clone()))
                            .await;
                        host.report_error(e).await;
                        // A streak of venue rejections is the trading-error
                        // signal the capability self-check exists for: probe
                        // the venue directly (rate-limited) so the freeze
                        // decision rests on fresh evidence, not inference.
                        if place_failures >= 3 && now_epoch_ms() >= next_probe_ok_ms {
                            next_probe_ok_ms = now_epoch_ms() + PROBE_COOLDOWN_MS;
                            match venue.self_check().await {
                                Ok(report) => host.on_self_check(report).await,
                                Err(e) => host.report_error(e).await,
                            }
                        }
                    }
                }
            }

            // 1b) Forward every core-side retire as a REAL venue cancel. The
            // core already flipped the order to Cancelled locally; a cancel
            // that never reaches the venue leaves a resting orphan.
            for id in host.take_pending_cancels().await {
                match venue.cancel(id.clone()).await {
                    Ok(()) => host.note_orphan_cancelled(&id).await,
                    Err(e) => {
                        eprintln!("polymarket-extension: venue cancel failed for {id}: {e}");
                        host.report_error(e).await;
                    }
                }
            }

            // 1c) Settlements: ask the venue what happened to every market the
            // core still holds. A resolved market closes its positions in the
            // core, which books the payout as a receivable; step 1d turns that
            // into cash. For a market that has not resolved the venue answers
            // "not resolved", which is what keeps the core's silence detection
            // measuring a live channel.
            let queries = host.take_settlement_queries().await;
            if !queries.is_empty() {
                match venue.resolutions(queries).await {
                    Ok(resolutions) => {
                        for resolution in resolutions {
                            host.on_market_resolution(resolution).await;
                        }
                    }
                    // Loud, but never fatal: a Gamma outage must not stop
                    // trading, and the core alerts on its own once it has waited
                    // too long for an answer.
                    Err(e) => eprintln!(
                        "polymarket-extension: settlement query failed: {e} (raw={:?})",
                        e.raw
                    ),
                }
            }

            // 1d) Redemptions. Each attempt signs and mines a real transaction,
            // so it runs OFF this loop (a receipt wait can outlast a block time,
            // and stalling here would stall order placement) and never more than
            // REDEEM_MAX_IN_FLIGHT at a time (one wallet, one nonce). A claim
            // that cannot run now is left for the core, which re-arms it on its
            // backoff — nothing is dropped.
            while redeems.len() < REDEEM_MAX_IN_FLIGHT && redeem_queue.is_empty() {
                let due = host.take_pending_redemptions().await;
                if due.is_empty() {
                    break;
                }
                redeem_queue.extend(due);
            }
            while redeems.len() < REDEEM_MAX_IN_FLIGHT {
                let Some(request) = redeem_queue.pop_front() else {
                    break;
                };
                let host = Arc::clone(&host);
                let redeem_cfg = redeem_cfg.clone();
                redeems.spawn(async move {
                    let outcome = match &redeem_cfg {
                        Ok(config) => redeem::redeem_claim(config, &request).await,
                        // Redemption cannot run in this process at all (no
                        // endpoint, or a holder the signer cannot move): say so
                        // as a MANUAL failure, which the core reports once and
                        // then leaves to the operator.
                        Err(failure) => RedeemOutcome::Failed {
                            message: failure.message.clone(),
                            manual: failure.manual,
                        },
                    };
                    let result = redeem::to_result(&request, outcome, now_epoch_ms());
                    host.on_redemption_result(result).await;
                });
            }
            while redeems.try_join_next().is_some() {}

            // 2) Drain venue events into the host.
            while let Ok(ev) = evt_rx.try_recv() {
                match ev {
                    VenueEvent::Fill(fill) => host.on_fill(fill).await,
                    VenueEvent::OrderLive { venue_order_id } => {
                        host.on_order_live(&venue_order_id).await
                    }
                    VenueEvent::OrderCancelled { venue_order_id } => {
                        host.on_order_cancelled(&venue_order_id).await
                    }
                    VenueEvent::ReconcileReport(_) => {}
                    VenueEvent::Fatal(msg) => {
                        eprintln!("polymarket-extension: venue fatal: {msg}");
                        host.report_error(CoreError::new(CoreErrorCode::VenueError, msg))
                            .await;
                    }
                }
            }

            // 3) Periodic REST reconciliation (every ~5s).
            since_reconcile += 1;
            if since_reconcile >= 10 {
                since_reconcile = 0;
                match venue.snapshot().await {
                    Ok((open_order_ids, trades)) => {
                        sweep_failures = 0;
                        // Orphan sweep: a venue-open id the core has never
                        // heard of (a POST the venue accepted but the bridge
                        // timed out on, or a resting order the core retired
                        // while the venue-side cancel failed) gets cancelled
                        // here — every sweep, not just at startup.
                        let known: std::collections::HashSet<String> =
                            host.known_venue_order_ids().await.into_iter().collect();
                        let now = now_epoch_ms();
                        recent_placements.retain(|(_, at)| now - *at < ORPHAN_GRACE_MS);
                        for id in open_order_ids.clone() {
                            if known.contains(&id)
                                || recent_placements.iter().any(|(rid, _)| *rid == id)
                            {
                                continue; // ours and tracked / just placed
                            }
                            match venue.cancel(id.clone()).await {
                                Ok(()) => {
                                    eprintln!("polymarket-extension: cancelled orphan order {id}");
                                    host.note_orphan_cancelled(&id).await;
                                }
                                Err(e) => {
                                    eprintln!(
                                        "polymarket-extension: failed to cancel orphan {id}: {e}"
                                    );
                                    host.report_error(CoreError::new(
                                        CoreErrorCode::VenueError,
                                        format!("orphan cancel failed for {id}: {e}"),
                                    ))
                                    .await;
                                }
                            }
                        }
                        host.on_reconcile(ReconcileSnapshot {
                            open_order_ids,
                            trades,
                            now_ms: now_ms(),
                        })
                        .await;
                    }
                    // Always loud on failure — a silent sweep is exactly the
                    // blind-safety-net failure mode this loop exists to catch.
                    // Three in a row means the sweep channel itself is broken:
                    // the host freezes trading on it (E31-b).
                    Err(e) => {
                        sweep_failures = sweep_failures.saturating_add(1);
                        eprintln!("polymarket-extension: sweep failed ({sweep_failures}): {e}");
                        if sweep_failures >= SWEEP_FAILURE_FREEZE {
                            host.on_reconcile_failed(e.clone()).await;
                        } else {
                            host.report_error(e).await;
                        }
                    }
                }
                // Cash truth: the ledger runs on fill deltas, so a fill the
                // bot misses also leaves the balance silently stale. Re-align
                // against the venue's free cash every sweep (the host gates
                // the correction on its own reserved amount).
                match venue.balance().await {
                    Ok(free) => host.venue_free_balance(free).await,
                    Err(e) => host.report_error(e).await,
                }
            }
        }
    });

    Ok(Some(handle))
}
