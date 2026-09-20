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
use crate::venue::{VenueEvent, spawn_from_env};
use blitzkrieg_market_api::{CoreError, CoreErrorCode, MarketHost, ReconcileSnapshot};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

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
        loop {
            tick.tick().await;

            // 1) Submit locally-accepted orders not yet on the venue.
            for order in host.take_pending_orders().await {
                let core_order_id = order.core_order_id.clone();
                match venue.place(order).await {
                    Ok(p) => {
                        host.on_order_accepted(&core_order_id, &p.venue_order_id)
                            .await
                    }
                    Err(e) => {
                        host.on_order_rejected(&core_order_id).await;
                        host.report_error(e).await;
                    }
                }
            }

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
                        host.on_reconcile(ReconcileSnapshot {
                            open_order_ids,
                            trades,
                            now_ms: now_ms(),
                        })
                        .await;
                    }
                    // Always loud on failure — a silent sweep is exactly the
                    // blind-safety-net failure mode this loop exists to catch.
                    Err(e) => {
                        eprintln!("polymarket-extension: sweep failed: {e}");
                        host.report_error(e).await;
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
