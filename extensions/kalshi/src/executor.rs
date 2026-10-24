//! The order executor: the market-plugin component that turns the core's
//! locally-accepted orders into real Kalshi orders and reconciles the venue's
//! own state back (stage 4 of the kalshi integration — the component #423/#424
//! left behind a "arrives with stage 4" note).
//!
//! Kalshi's portfolio API has no private WebSocket push, so the executor is a
//! REST polling actor on the same shape as the feed: one tokio task owns a
//! credentialed [`KalshiRest`], drains the host's pending orders / cancels
//! every tick, and runs a periodic reconciliation sweep (open orders + fills +
//! orphan cleanup + free-balance sync). Everything the core sees crosses the
//! `MarketHost` seam; all Kalshi knowledge stays here.
//!
//! Unit conventions at the boundary:
//!   * price — the core speaks 0..1 probabilities; Kalshi speaks cents. The
//!     conversion is the feed's `cents_to_prob` in reverse, rounded to whole
//!     cents (a sub-cent limit would be silently rounded by the venue anyway).
//!   * side — YES↔UP, NO↔DOWN (the discovery convention): a token id is the
//!     ticker itself (YES) or `ticker` + `:no` (NO). A BUY on the YES token
//!     is action=buy side=yes; a BUY on the NO token is action=buy side=no.
//!   * fills — the fills endpoint reports per-match rows keyed by ticker +
//!     yes/no price; the token id is re-derived with the same convention so
//!     the core's OME matches them against its own orders.

use crate::discovery::NO_SUFFIX;
use crate::feed::cents_to_prob;
use crate::into_core_error;
use crate::rest::{Credentials, KalshiRest, RestConfig};
use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, ExecutorConfig, FillStatus, MarketFill, MarketHost,
    PendingOrder, ReconcileSnapshot, SelfCheckItem, SelfCheckReport, Side, VenueTradeInfo,
    net::now_ms,
};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};

/// Executor tick: pending-order/cancel drain cadence. The venue is REST; a
/// faster tick buys nothing and a slower one adds latency to every placement.
const EXECUTOR_TICK_MS: u64 = 500;
/// Reconciliation sweep every 10 ticks (~5s), the polymarket bridge's cadence.
const SWEEP_EVERY_TICKS: u64 = 10;
/// Consecutive sweep failures before the host freezes trading (E31-b: the
/// safety net being blind is worse than it being slow).
const SWEEP_FAILURE_FREEZE: u32 = 3;
/// A venue rejection streak that triggers a direct capability probe instead
/// of letting the freeze rest on inference.
const PLACE_FAILURE_PROBE: u32 = 3;

/// Build the credentialed client, or `None` when the environment carries no
/// key pair — the executor is then INERT (dry runs, or live without kalshi
/// credentials). Same contract as the polymarket bridge's `spawn_if_configured`.
fn credentials_from_env() -> Option<Credentials> {
    let key_id = std::env::var("KALSHI_API_KEY_ID").ok()?;
    let key_pem = std::env::var("KALSHI_RSA_PRIVATE_KEY").ok()?;
    if key_id.is_empty() || key_pem.is_empty() {
        return None;
    }
    Some(Credentials { key_id, key_pem })
}

fn rest_config() -> RestConfig {
    let mut config = RestConfig::default();
    if std::env::var("KALSHI_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    config
}

/// `0..1` probability → whole cents for the venue's limit price. A price the
/// venue could not represent (outside 1..=99) is refused BEFORE anything is
/// sent — a zero or 100-cent limit is a fat-finger, not an order.
fn prob_to_cents(price: Decimal) -> CoreResult<u64> {
    let cents = (price * Decimal::from(100)).round();
    if cents < Decimal::from(1) || cents > Decimal::from(99) {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("kalshi limit {price} does not map to 1..=99 cents"),
        ));
    }
    u64::try_from(cents)
        .map_err(|_| CoreError::new(CoreErrorCode::InvalidParams, "unreachable: 1..=99 cents"))
}

/// Split a core token id back into (ticker, side_yes) with the discovery
/// convention. Unknown shapes are refused, never guessed.
fn token_to_ticker_side(token_id: &str) -> CoreResult<(String, bool)> {
    match token_id.strip_suffix(NO_SUFFIX) {
        Some(ticker) if !ticker.is_empty() => Ok((ticker.to_string(), false)),
        None if !token_id.is_empty() && !token_id.contains(':') => Ok((token_id.to_string(), true)),
        _ => Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("token id {token_id} is not a kalshi ticker"),
        )),
    }
}

/// Re-derive the core token id from a fill row (the inverse of the above).
fn fill_token_id(ticker: &str, side_yes: bool) -> String {
    if side_yes {
        ticker.to_string()
    } else {
        format!("{ticker}{NO_SUFFIX}")
    }
}

/// Which side of the book a fill's price belongs to: the venue reports the
/// executed price as either a yes or a no price; exactly one is present.
fn fill_is_yes(fill: &crate::rest::KalshiFill) -> bool {
    if fill.yes_price.is_some() {
        true
    } else {
        fill.no_price.is_none() || fill.side.trim().eq_ignore_ascii_case("yes")
    }
}

fn fill_price_cents(fill: &crate::rest::KalshiFill) -> Option<Decimal> {
    if fill_is_yes(fill) {
        fill.yes_price
    } else {
        fill.no_price
    }
}

/// Kalshi's ISO close/created timestamps → epoch millis (the discovery
/// parser, reused so the two components share one narrow shape).
fn created_to_ms(raw: &Option<String>) -> i64 {
    raw.as_deref()
        .and_then(crate::discovery::close_time_to_ms)
        .unwrap_or_else(now_ms)
}

/// Map one fill row onto the seam type. A row whose price or shape cannot be
/// read is skipped by the caller with a log line — never forwarded as garbage.
fn to_venue_trade(fill: &crate::rest::KalshiFill) -> Option<VenueTradeInfo> {
    let side_yes = fill_is_yes(fill);
    let cents = fill_price_cents(fill)?;
    let prob = cents_to_prob(cents)?;
    let size = fill.count;
    if size <= Decimal::ZERO {
        return None;
    }
    Some(VenueTradeInfo {
        venue_order_id: fill.order_id.clone(),
        trade_id: fill.trade_id.clone(),
        token_id: fill_token_id(&fill.ticker, side_yes),
        side: Side::Buy,
        size,
        price: prob,
        ts_ms: created_to_ms(&fill.created_time),
        tx_hash: None,
        maker: None,
    })
}

fn to_fill(order_id: &str, trade: &VenueTradeInfo) -> MarketFill {
    MarketFill {
        order_id: order_id.to_string(),
        trade_id: Some(trade.trade_id.clone()),
        token_id: trade.token_id.clone(),
        side: trade.side,
        price: trade.price,
        size: trade.size,
        status: FillStatus::Matched,
        ts_ms: trade.ts_ms,
        tx_hash: None,
        maker: trade.maker,
    }
}

/// Spawn the executor. Inert (Ok(None)) without credentials; otherwise a
/// background task runs until the process ends.
pub async fn spawn_if_configured(
    host: Arc<dyn MarketHost>,
    _config: ExecutorConfig,
) -> CoreResult<Option<tokio::task::JoinHandle<()>>> {
    let Some(credentials) = credentials_from_env() else {
        return Ok(None);
    };
    let client = Arc::new(
        KalshiRest::new(rest_config(), Some(credentials)).map_err(|e| {
            CoreError::new(CoreErrorCode::VenueError, format!("kalshi client: {e}"))
        })?,
    );
    let handle = tokio::spawn(executor_loop(host, client));
    Ok(Some(handle))
}

/// Test-only entry for the executor integration suite: drive the REAL
/// production loop against a mock server with an explicit client (env-var
/// plumbing stays out of the test's control, and no credential is fixed in
/// source — the suite generates a throwaway RSA key in memory). Not part of
/// the public surface, exactly like [`crate::feed::feed_tests_only_spawn`].
#[doc(hidden)]
pub fn executor_tests_only_spawn(host: Arc<dyn MarketHost>, client: Arc<KalshiRest>) {
    tokio::spawn(executor_loop(host, client));
}

/// One capability probe against the paths trading actually uses: balance
/// (auth), open orders (portfolio read) and a one-second fills page. A failed
/// item means order egress cannot work either.
async fn self_check(client: &KalshiRest) -> SelfCheckReport {
    let items = vec![
        {
            let ok = client.balance().await.is_ok();
            SelfCheckItem {
                name: "balance".into(),
                ok,
                detail: if ok {
                    "balance endpoint answered".into()
                } else {
                    "balance failed (credentials or venue down)".into()
                },
            }
        },
        {
            let ok = client.orders(None).await.is_ok();
            SelfCheckItem {
                name: "orders".into(),
                ok,
                detail: if ok {
                    "portfolio orders answered".into()
                } else {
                    "portfolio orders failed".into()
                },
            }
        },
        {
            let ok = client.fills(None).await.is_ok();
            SelfCheckItem {
                name: "fills".into(),
                ok,
                detail: if ok {
                    "fills answered".into()
                } else {
                    "fills failed".into()
                },
            }
        },
    ];
    SelfCheckReport {
        ok: items.iter().all(|i| i.ok),
        ts_ms: now_ms(),
        items,
    }
}

pub(crate) async fn executor_loop(host: Arc<dyn MarketHost>, client: Arc<KalshiRest>) {
    let mut tick = interval(Duration::from_millis(EXECUTOR_TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut since_sweep: u64 = 0;
    let mut sweep_failures: u32 = 0;
    let mut place_failures: u32 = 0;
    // Fill rows already reported to the host (the fills endpoint pages by
    // timestamp; dedup by trade id so a sweep overlap never double-books).
    let mut seen_trades: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Orders just placed, still absent from the venue's open-order list (the
    // orphan sweep's grace window).
    let mut recent_placements: Vec<(String, i64)> = Vec::new();
    const ORPHAN_GRACE_MS: i64 = 30_000;

    // Seed the ledger from the venue so the reserve gate reflects real cash.
    match client.balance().await {
        Ok(b) => {
            host.seed_balance("kalshi", b.balance).await;
            eprintln!("kalshi-extension: executor ready balance={}", b.balance);
        }
        Err(e) => eprintln!("kalshi-extension: live balance fetch failed: {e}"),
    }
    let report = self_check(&client).await;
    if !report.ok {
        eprintln!("kalshi-extension: startup self-check FAILED");
    }
    host.on_self_check(report).await;

    // Startup orphan sweep: anything resting at the venue the core does not
    // know about is cancelled before trading resumes.
    match client.orders(None).await {
        Ok(open) => {
            let known: std::collections::HashSet<String> =
                host.known_venue_order_ids().await.into_iter().collect();
            for o in &open {
                if known.contains(&o.order_id) {
                    continue;
                }
                if let Err(e) = client.cancel_order(&o.order_id).await {
                    eprintln!("kalshi-extension: orphan cancel {}: {e}", o.order_id);
                } else {
                    host.note_orphan_cancelled(&o.order_id).await;
                }
            }
        }
        Err(e) => eprintln!("kalshi-extension: startup orphan sweep failed: {e}"),
    }

    loop {
        tick.tick().await;

        // 1) Cancels first (same serialization rule as the polymarket bridge:
        //    the old order is gone before a replacement is submitted).
        let mut cancel_strikes = 0u32;
        for id in host.take_pending_cancels("kalshi").await {
            match client.cancel_order(&id).await {
                Ok(_) => host.note_orphan_cancelled(&id).await,
                Err(e) => {
                    cancel_strikes += 1;
                    eprintln!("kalshi-extension: venue cancel failed for {id}: {e}");
                    host.report_error(into_core_error(&e)).await;
                }
            }
        }

        // 2) Submit locally-accepted orders. `take_pending_orders` routes by
        //    this plugin's venue name — with multiple executors running, each
        //    drains only ITS venue's orders.
        if cancel_strikes == 0 {
            for order in host.take_pending_orders("kalshi").await {
                match place(&client, &order).await {
                    Ok(venue_id) => {
                        place_failures = 0;
                        recent_placements.push((venue_id.clone(), now_ms()));
                        host.on_order_accepted(&order.core_order_id, &venue_id)
                            .await;
                    }
                    Err(e) if e.code == CoreErrorCode::Timeout => {
                        place_failures += 1;
                        host.on_place_timeout(&order.core_order_id).await;
                        host.report_error(e).await;
                    }
                    Err(e) => {
                        place_failures += 1;
                        host.on_order_rejected(&order.core_order_id, Some(e.clone()))
                            .await;
                        host.report_error(e).await;
                        if place_failures >= PLACE_FAILURE_PROBE {
                            place_failures = 0;
                            host.on_self_check(self_check(&client).await).await;
                        }
                    }
                }
            }
        }

        // 3) Periodic reconciliation sweep.
        since_sweep += 1;
        if since_sweep < SWEEP_EVERY_TICKS {
            continue;
        }
        since_sweep = 0;
        match sweep(
            &host,
            &client,
            &mut seen_trades,
            &mut recent_placements,
            ORPHAN_GRACE_MS,
        )
        .await
        {
            Ok(()) => sweep_failures = 0,
            Err(e) => {
                sweep_failures += 1;
                eprintln!("kalshi-extension: sweep failed ({sweep_failures}): {e}");
                if sweep_failures >= SWEEP_FAILURE_FREEZE {
                    host.on_reconcile_failed(e).await;
                } else {
                    host.report_error(e).await;
                }
            }
        }
    }
}

/// Submit one order. The core's `core_order_id` becomes the client order id
/// (the venue's idempotency key), so a timeout can be reconciled by querying
/// orders for it — never blindly retried.
async fn place(client: &KalshiRest, order: &PendingOrder) -> CoreResult<String> {
    let (ticker, side_yes) = token_to_ticker_side(&order.token_id)?;
    let cents = prob_to_cents(order.price)?;
    let count = u64::try_from(order.size.trunc()).unwrap_or(0);
    if count == 0 {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "kalshi order count rounds to zero",
        ));
    }
    let action_buy = order.side == Side::Buy;
    let ack = client
        .place_limit_order(
            &ticker,
            side_yes,
            action_buy,
            count,
            cents,
            &order.core_order_id,
            None,
        )
        .await
        .map_err(|e| into_core_error(&e))?;
    Ok(ack.order_id)
}

/// One reconciliation pass: open orders + new fills + orphan cleanup + free
/// balance. Any transport failure aborts the pass so the failure counter sees
/// a coherent "sweep did not complete", not a half-swept ledger.
async fn sweep(
    host: &Arc<dyn MarketHost>,
    client: &KalshiRest,
    seen_trades: &mut std::collections::HashSet<String>,
    recent_placements: &mut Vec<(String, i64)>,
    orphan_grace_ms: i64,
) -> CoreResult<()> {
    let open = client.orders(None).await.map_err(|e| into_core_error(&e))?;
    let fills = client.fills(None).await.map_err(|e| into_core_error(&e))?;

    // Orphan sweep: a venue-open id the core has never heard of gets cancelled
    // (a POST accepted after our timeout, or a resting order whose cancel
    // failed). Fresh placements ride out a grace window.
    let known: std::collections::HashSet<String> =
        host.known_venue_order_ids().await.into_iter().collect();
    let now = now_ms();
    recent_placements.retain(|(_, at)| now - *at < orphan_grace_ms);
    for o in &open {
        if known.contains(&o.order_id)
            || recent_placements.iter().any(|(rid, _)| *rid == o.order_id)
        {
            continue;
        }
        match client.cancel_order(&o.order_id).await {
            Ok(_) => {
                eprintln!("kalshi-extension: cancelled orphan order {}", o.order_id);
                host.note_orphan_cancelled(&o.order_id).await;
            }
            Err(e) => {
                eprintln!(
                    "kalshi-extension: failed to cancel orphan {}: {e}",
                    o.order_id
                );
                host.report_error(CoreError::new(
                    CoreErrorCode::VenueError,
                    format!("orphan cancel failed for {}: {e}", o.order_id),
                ))
                .await;
            }
        }
    }

    // Fills → reconcile snapshot; the core matches them to its own orders and
    // books what it owns. Kalshi fills carry the ticker + executed side price;
    // rows we cannot read are logged and skipped, never forwarded as garbage.
    let mut trades = Vec::new();
    for fill in &fills {
        if seen_trades.contains(&fill.trade_id) {
            continue;
        }
        match to_venue_trade(fill) {
            Some(trade) => {
                seen_trades.insert(fill.trade_id.clone());
                trades.push(trade);
            }
            None => {
                eprintln!(
                    "kalshi-extension: unreadable fill row {} (ticker {}) — skipped",
                    fill.trade_id, fill.ticker
                );
            }
        }
    }
    let snapshot = ReconcileSnapshot {
        open_order_ids: open.iter().map(|o| o.order_id.clone()).collect(),
        trades: trades.clone(),
        now_ms: now_ms(),
    };
    host.on_reconcile(snapshot).await;
    for trade in &trades {
        host.on_fill(to_fill(&trade.venue_order_id, trade)).await;
    }

    // Cash truth: re-align the ledger's free view every sweep.
    match client.balance().await {
        Ok(b) => host.venue_free_balance("kalshi", b.balance).await,
        Err(e) => host.report_error(into_core_error(&e)).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prob_cents_round_trip_on_the_grid() {
        // The feed maps cents → prob; the executor maps back. Whole cents are
        // exact; a sub-cent core price rounds to the nearest cent.
        assert_eq!(prob_to_cents(Decimal::new(43, 2)).unwrap(), 43);
        assert_eq!(prob_to_cents(Decimal::new(99, 2)).unwrap(), 99);
        assert_eq!(prob_to_cents(Decimal::new(1, 2)).unwrap(), 1);
        // 0.435 rounds to 44 (half-up), the venue grid.
        assert_eq!(prob_to_cents(Decimal::new(435, 3)).unwrap(), 44);
    }

    #[test]
    fn prob_cents_refuses_off_grid_prices() {
        // 0 and 1 are not tradable limits on kalshi (1..=99 cents only).
        assert!(prob_to_cents(Decimal::ZERO).is_err());
        assert!(prob_to_cents(Decimal::ONE).is_err());
        assert!(prob_to_cents(Decimal::new(150, 2)).is_err());
    }

    #[test]
    fn token_ids_round_trip_with_the_discovery_convention() {
        let (ticker, yes) = token_to_ticker_side("KXBTC-26-OCT05").unwrap();
        assert_eq!(ticker, "KXBTC-26-OCT05");
        assert!(yes);
        let (ticker, yes) = token_to_ticker_side("KXBTC-26-OCT05:no").unwrap();
        assert_eq!(ticker, "KXBTC-26-OCT05");
        assert!(!yes);
        // A colon shape that is not the NO suffix is refused, not guessed.
        assert!(token_to_ticker_side("weird:id").is_err());
        assert!(token_to_ticker_side("").is_err());
        // And the fill side rebuilds exactly the ids discovery minted.
        assert_eq!(fill_token_id("KXBTC-26-OCT05", true), "KXBTC-26-OCT05");
        assert_eq!(fill_token_id("KXBTC-26-OCT05", false), "KXBTC-26-OCT05:no");
    }

    #[test]
    fn fill_row_mapping_reports_side_price_and_token() {
        let yes_fill = crate::rest::KalshiFill {
            trade_id: "t1".into(),
            order_id: "o1".into(),
            ticker: "KXBTC-26-OCT05".into(),
            side: "yes".into(),
            action: "buy".into(),
            count: Decimal::from(5),
            yes_price: Some(Decimal::from(43)),
            no_price: None,
            created_time: Some("2026-10-14T10:00:00Z".into()),
        };
        let trade = to_venue_trade(&yes_fill).expect("yes fill maps");
        assert_eq!(trade.token_id, "KXBTC-26-OCT05");
        assert_eq!(trade.price, Decimal::new(43, 2));
        assert_eq!(trade.size, Decimal::from(5));

        let no_fill = crate::rest::KalshiFill {
            trade_id: "t2".into(),
            order_id: "o2".into(),
            ticker: "KXBTC-26-OCT05".into(),
            side: "no".into(),
            action: "buy".into(),
            count: Decimal::from(3),
            yes_price: None,
            no_price: Some(Decimal::from(57)),
            created_time: None,
        };
        let trade = to_venue_trade(&no_fill).expect("no fill maps");
        assert_eq!(trade.token_id, "KXBTC-26-OCT05:no");
        assert_eq!(trade.price, Decimal::new(57, 2));
    }

    #[test]
    fn zero_count_fill_rows_are_skipped() {
        let zero = crate::rest::KalshiFill {
            trade_id: "t3".into(),
            order_id: "o3".into(),
            ticker: "KXBTC-26-OCT05".into(),
            side: "yes".into(),
            action: "buy".into(),
            count: Decimal::ZERO,
            yes_price: Some(Decimal::from(43)),
            no_price: None,
            created_time: None,
        };
        assert!(to_venue_trade(&zero).is_none());
    }
}
