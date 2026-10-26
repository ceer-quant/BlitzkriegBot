//! The order executor: the market-plugin component that turns the core's
//! locally-accepted orders into real PolySimulator orders and reconciles the
//! venue's own state back (#454).
//!
//! The venue has no private push channel the extension consumes (its user WS
//! needs JWT rotation and the free tier allows one connection), so the
//! executor is a REST polling actor on the same shape as the kalshi
//! executor: one tokio task owns a credentialed [`PolySimRest`], drains the
//! host's pending orders / cancels every tick, and runs a periodic
//! reconciliation sweep (open orders + trades + orphan cleanup + free-balance
//! sync). Everything the core sees crosses the `MarketHost` seam; all
//! PolySimulator knowledge stays here.
//!
//! Unit conventions at the boundary:
//!   * price — the core speaks 0..1 probabilities; this venue quotes dollars
//!     0..1 too, so prices pass through. An off-tick limit is answered with
//!     `INVALID_ORDER_MIN_TICK_SIZE` rather than silently reinterpreted.
//!   * order form — the venue's NATIVE place is the `{"order":{tokenId,
//!     side, makerAmount, takerAmount}}` envelope with 1e6-scaled integer
//!     strings (price = maker/taker for BUY, inverted for SELL); outcome,
//!     quantity and time-in-force do not exist as fields — the venue derives
//!     them from the token and the amounts (verified live 2026-10-25: the
//!     flat form is answered `VALIDATION_FAILED: missing order`).
//!   * fills — `GET /v1/data/trades` reports PM-shape rows whose legs split
//!     across `taker_order_id` (the marketable side) and `maker_orders` (the
//!     resting ids consumed). OUR fill books under whichever of the two names
//!     our venue order id; the venue order id is an INTEGER everywhere,
//!     stringified at this seam.

use crate::into_core_error;
use crate::rest::{Credentials, PolySimRest, RestConfig};
use crate::types::{PlaceOrderBody, UpDownRow};
use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, ExecutorConfig, FillStatus, MarketFill, MarketHost,
    MarketResolution, PendingOrder, ReconcileSnapshot, SelfCheckItem, SelfCheckReport,
    SettlementQuery, Side, VenueTradeInfo, net::now_ms,
};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};

/// Executor tick: pending-order/cancel drain cadence. The venue is REST and
/// per-user serializes order writes; a faster tick buys nothing.
const EXECUTOR_TICK_MS: u64 = 500;
/// Reconciliation sweep every 10 ticks (~5s), the sibling executors' cadence.
const SWEEP_EVERY_TICKS: u64 = 10;
/// Consecutive sweep failures before the host freezes trading (E31-b: the
/// safety net being blind is worse than it being slow).
const SWEEP_FAILURE_FREEZE: u32 = 3;
/// A venue rejection streak that triggers a direct capability probe instead
/// of letting the freeze rest on inference.
const PLACE_FAILURE_PROBE: u32 = 3;

/// Build the credentialed client, or `None` when the environment carries no
/// API key — the executor is then INERT (dry runs). Same contract as the
/// sibling executors' `spawn_if_configured`.
fn credentials_from_env() -> Option<Credentials> {
    let api_key = std::env::var("POLYSIM_API_KEY").ok()?;
    if api_key.is_empty() {
        return None;
    }
    Some(Credentials { api_key })
}

fn rest_config() -> RestConfig {
    let mut config = RestConfig::default();
    if std::env::var("POLYSIM_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    config
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
        PolySimRest::new(rest_config(), Some(credentials)).map_err(|e| {
            CoreError::new(
                CoreErrorCode::VenueError,
                format!("polysimulator client: {e}"),
            )
        })?,
    );
    let handle = tokio::spawn(executor_loop(host, client));
    Ok(Some(handle))
}

/// Test-only entry for the executor integration suite: drive the REAL
/// production loop against a mock server with an explicit client (env-var
/// plumbing stays out of the test's control, and no credential is fixed in
/// source). Not part of the public surface, exactly like the feed's
/// `feed_tests_only_spawn`.
#[doc(hidden)]
pub fn executor_tests_only_spawn(host: Arc<dyn MarketHost>, client: Arc<PolySimRest>) {
    tokio::spawn(executor_loop(host, client));
}

/// One capability probe against the paths trading actually uses: balance
/// (auth), open orders and a one-page trades query. A failed item means
/// order egress cannot work either.
/// One capability probe against the paths trading actually uses: balance
/// (auth), open orders and a one-page trades query. A failed item means
/// order egress cannot work either.
async fn self_check(client: &PolySimRest) -> SelfCheckReport {
    // Each probe's failure reason rides in the detail: a freeze whose only
    // text is "trades failed" sent an operator hunting blind (the venue's
    // "Rate limit exceeded. Retry after 1s." is a different story than an
    // auth rejection, and #184 says the reason goes on the line).
    let balance = client.balance().await;
    let orders = client.open_orders().await;
    let trades = client.trades().await;
    let items = vec![
        SelfCheckItem {
            name: "balance".into(),
            ok: balance.is_ok(),
            detail: match balance {
                Ok(_) => "balance endpoint answered".into(),
                Err(e) => format!("balance failed: {e}"),
            },
        },
        SelfCheckItem {
            name: "orders".into(),
            ok: orders.is_ok(),
            detail: match orders {
                Ok(_) => "open orders answered".into(),
                Err(e) => format!("open orders failed: {e}"),
            },
        },
        SelfCheckItem {
            name: "trades".into(),
            ok: trades.is_ok(),
            detail: match trades {
                Ok(_) => "trades answered".into(),
                Err(e) => format!("trades failed: {e}"),
            },
        },
    ];
    SelfCheckReport {
        ok: items.iter().all(|i| i.ok),
        ts_ms: now_ms(),
        items,
    }
}

pub(crate) async fn executor_loop(host: Arc<dyn MarketHost>, client: Arc<PolySimRest>) {
    let mut tick = interval(Duration::from_millis(EXECUTOR_TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut since_sweep: u64 = 0;
    let mut sweep_failures: u32 = 0;
    let mut place_failures: u32 = 0;
    // Trade rows already reported to the host (dedup by trade id so a sweep
    // overlap never double-books).
    let mut seen_trades: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Orders just placed, still absent from the venue's open-order list (the
    // orphan sweep's grace window).
    let mut recent_placements: Vec<(String, i64)> = Vec::new();
    const ORPHAN_GRACE_MS: i64 = 30_000;

    // Seed the ledger from the venue so the reserve gate reflects real cash.
    match client.balance().await {
        Ok(b) => {
            host.seed_balance("polysimulator", b.balance).await;
            eprintln!(
                "polysimulator-extension: executor ready balance={}",
                b.balance
            );
        }
        Err(e) => eprintln!("polysimulator-extension: live balance fetch failed: {e}"),
    }
    let report = self_check(&client).await;
    if !report.ok {
        eprintln!("polysimulator-extension: startup self-check FAILED");
    }
    host.on_self_check("polysimulator", report).await;

    // Startup orphan sweep: anything resting at the venue the core does not
    // know about is cancelled before trading resumes.
    match client.open_orders().await {
        Ok(open) => {
            let known: std::collections::HashSet<String> =
                host.known_venue_order_ids().await.into_iter().collect();
            for o in &open {
                let vid = o.venue_id();
                if known.contains(&vid) {
                    continue;
                }
                if let Err(e) = client.cancel_order(&vid).await {
                    eprintln!("polysimulator-extension: orphan cancel {vid}: {e}");
                } else {
                    host.note_orphan_cancelled(&vid).await;
                }
            }
        }
        Err(e) => eprintln!("polysimulator-extension: startup orphan sweep failed: {e}"),
    }

    loop {
        tick.tick().await;

        // 1) Cancels first (same serialization rule as the sibling executors:
        //    the old order is gone before a replacement is submitted).
        let mut cancel_strikes = 0u32;
        for id in host.take_pending_cancels("polysimulator").await {
            match client.cancel_order(&id).await {
                Ok(_) => host.note_orphan_cancelled(&id).await,
                Err(e) => {
                    cancel_strikes += 1;
                    eprintln!("polysimulator-extension: venue cancel failed for {id}: {e}");
                    host.report_error(into_core_error(&e)).await;
                }
            }
        }

        // 2) Submit locally-accepted orders. `take_pending_orders` routes by
        //    this plugin's venue name — with multiple executors running, each
        //    drains only ITS venue's orders.
        if cancel_strikes == 0 {
            for order in host.take_pending_orders("polysimulator").await {
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
                            host.on_self_check("polysimulator", self_check(&client).await)
                                .await;
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
                eprintln!("polysimulator-extension: sweep failed ({sweep_failures}): {e}");
                if sweep_failures >= SWEEP_FAILURE_FREEZE {
                    host.on_reconcile_failed("polysimulator", e).await;
                } else {
                    host.report_error(e).await;
                }
            }
        }

        // 4) Settlements (root cause C): the core asks what happened to every
        //    market it still holds a position in; without an answer a
        //    held-to-expiry position stays a ghost (proceeds 0, expiry past),
        //    the exit ladder keeps re-emitting SELLs for a dead token, the
        //    venue rejects with "Insufficient position", and the rejection
        //    streak freezes the account. The updown list already carries the
        //    verdict: resolved=true with terminal 1/0 outcome prices. One
        //    fetch per sweep cycle, only when there ARE queries; a query the
        //    list does not name or has not resolved yet answers nothing — the
        //    core re-asks on its own cadence, and payouts are never invented.
        //    Full page on purpose: settlement needs the whole list as its
        //    bottom line (traffic diet lives in discovery, #637).
        let queries = host.take_settlement_queries().await;
        if queries.is_empty() {
            continue;
        }
        match client.updown_markets(None, None).await {
            Ok(rows) => {
                for query in &queries {
                    if let Some(resolution) = resolution_for(query, &rows) {
                        eprintln!(
                            "polysimulator-extension: settlement {} payouts={:?}",
                            resolution.condition_id, resolution.payouts
                        );
                        host.on_market_resolution(resolution).await;
                    }
                }
            }
            // Loud, but never fatal: a failed fetch must not stop trading,
            // and the core alerts on its own once it has waited too long.
            Err(e) => eprintln!(
                "polysimulator-extension: settlement query ({}) fetch failed: {e}",
                queries.len()
            ),
        }
    }
}

/// The venue's native body for one core order: side/amounts derived per the
/// live contract (price = maker/taker BUY, taker/maker SELL; 1e6 scaling).
/// Split from `place` so the scaling math is testable without a client.
fn place_body_for(side: &Side, price: Decimal, shares: Decimal, token_id: &str) -> PlaceOrderBody {
    let scale = Decimal::from(1_000_000u64);
    let (maker_amount, taker_amount) = match side {
        Side::Buy => (price * shares, shares),
        Side::Sell => (shares, price * shares),
    };
    PlaceOrderBody {
        token_id: token_id.to_string(),
        side: match side {
            Side::Buy => "BUY".to_string(),
            Side::Sell => "SELL".to_string(),
        },
        maker_amount: (maker_amount * scale).trunc().to_string(),
        taker_amount: (taker_amount * scale).trunc().to_string(),
    }
}

/// Submit one order. The venue's native form is (tokenId, side, makerAmount,
/// takerAmount) — price = maker/taker for BUY and taker/maker for SELL,
/// amounts 1e6-scaled integer strings. The core's `core_order_id` cannot ride
/// along usefully: the venue accepts a `client_order_id` field but neither
/// echoes nor dedupes on it (verified live 2026-10-25), so a timeout is
/// reconciled through the orders list, never by blind retry. Outcome,
/// quantity and time-in-force do not exist as fields — the venue derives them
/// from the token and the amounts.
async fn place(client: &PolySimRest, order: &PendingOrder) -> CoreResult<String> {
    if order.size < Decimal::ONE {
        return Err(CoreError::new(
            CoreErrorCode::InvalidParams,
            "polysimulator order size rounds to zero",
        ));
    }
    let shares = order.size.trunc();
    let body = place_body_for(&order.side, order.price, shares, &order.token_id);
    let ack = client
        .place_order(&body)
        .await
        .map_err(|e| into_core_error(&e))?;
    Ok(ack.venue_id())
}

/// Attribute one trade row to our order ids: a row is OURS when our venue id
/// appears as the taker (we crossed) or inside the maker set (someone crossed
/// our resting order). Returns the venue order id that books the fill plus
/// whether it was the maker leg — the fill's maker flag is decided by WHICH
/// leg matched, not by which fields happen to be present.
fn our_leg(
    row: &crate::types::PmTradeRow,
    our_ids: &std::collections::HashSet<String>,
) -> Option<(String, bool)> {
    if let Some(taker) = &row.taker_order_id
        && our_ids.contains(taker)
    {
        return Some((taker.clone(), false));
    }
    if let Some(makers) = &row.maker_orders {
        for m in makers {
            if our_ids.contains(m) {
                return Some((m.clone(), true));
            }
        }
    }
    None
}

/// Re-derive the core token id from a trade row: the row's `asset_id` IS the
/// CLOB token id on this venue (same id the descriptors minted).
fn row_token_id(row: &crate::types::PmTradeRow) -> Option<String> {
    row.asset_id.clone().filter(|s| !s.is_empty())
}

/// Map one trade row onto the seam type. A row whose price or shape cannot be
/// read is skipped by the caller with a log line — never forwarded as garbage.
fn to_venue_trade(
    row: &crate::types::PmTradeRow,
    venue_order_id: String,
    maker: bool,
) -> Option<VenueTradeInfo> {
    let price = row.price?;
    let size = row.size?;
    if size <= Decimal::ZERO || price <= Decimal::ZERO || price >= Decimal::ONE {
        return None;
    }
    let side = match row.side.as_deref().map(str::to_ascii_uppercase).as_deref() {
        Some("SELL") => Side::Sell,
        _ => Side::Buy,
    };
    Some(VenueTradeInfo {
        venue_order_id,
        trade_id: row.id.clone(),
        token_id: row_token_id(row)?,
        side,
        size,
        price,
        // Live venue (2026-10-26) reports match_time in SECONDS since epoch
        // (e.g. 1792995878 for a 2026-10-26 fill); every ts_ms consumer treats
        // this field as MILLISECONDS. Normalize seconds → ms at the boundary;
        // a value already in ms passes through unchanged. Without this the
        // kernel's external-fill watermark (kept in ms) never advances past
        // these fills and they re-fold as external fills on every sweep.
        ts_ms: match row.match_time {
            Some(t) if t >= 1_000_000_000_000 => t,
            Some(t) => t.saturating_mul(1_000),
            None => now_ms(),
        },
        tx_hash: row.transaction_hash.clone(),
        maker: Some(maker),
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
        tx_hash: trade.tx_hash.clone(),
        maker: trade.maker,
    }
}

/// The venue's verdict for one settlement query, read off the updown rows:
/// a query whose condition_id matches a RESOLVED row pays its outcome prices
/// (a binary market pays 1/0); a query still open, unknown to the list, or
/// carrying unreadable / not-yet-0-or-1 prices answers NOTHING — the core
/// keeps watching and re-asks on its own cadence, and nothing is fabricated.
/// Split from the loop so the mapping is testable without a client.
fn resolution_for(query: &SettlementQuery, rows: &[UpDownRow]) -> Option<MarketResolution> {
    let row = rows.iter().find(|r| r.condition_id == query.condition_id)?;
    if !row.resolved.unwrap_or(false) {
        return None; // still live: the core re-queries
    }
    let (up_token, down_token) = row.parse_token_ids()?;
    let (up_price, down_price) = row.parse_outcome_prices()?;
    // Resolved means terminal prices: exactly 1 and 0. Anything else
    // (mid-round snapshot, half-parse) is not a verdict — answer nothing.
    let terminal = |p: Decimal| p == Decimal::ZERO || p == Decimal::ONE;
    if !terminal(up_price) || !terminal(down_price) {
        return None;
    }
    Some(MarketResolution {
        condition_id: query.condition_id.clone(),
        resolved: true,
        payouts: vec![(up_token, up_price), (down_token, down_price)],
        neg_risk: false,
        source: "polysimulator".to_string(),
    })
}

/// One reconciliation pass: open orders + new trades + orphan cleanup + free
/// balance. Any transport failure aborts the pass so the failure counter sees
/// a coherent "sweep did not complete", not a half-swept ledger.
async fn sweep(
    host: &Arc<dyn MarketHost>,
    client: &PolySimRest,
    seen_trades: &mut std::collections::HashSet<String>,
    recent_placements: &mut Vec<(String, i64)>,
    orphan_grace_ms: i64,
) -> CoreResult<()> {
    let open = client
        .open_orders()
        .await
        .map_err(|e| into_core_error(&e))?;
    let trades = client.trades().await.map_err(|e| into_core_error(&e))?;

    // Orphan sweep: a venue-open id the core has never heard of gets cancelled
    // (a POST accepted after our timeout, or a resting order whose cancel
    // failed). Fresh placements ride out a grace window.
    let known: std::collections::HashSet<String> =
        host.known_venue_order_ids().await.into_iter().collect();
    let now = now_ms();
    recent_placements.retain(|(_, at)| now - *at < orphan_grace_ms);
    for o in &open {
        let vid = o.venue_id();
        if known.contains(&vid) || recent_placements.iter().any(|(rid, _)| *rid == vid) {
            continue;
        }
        match client.cancel_order(&vid).await {
            Ok(_) => {
                eprintln!("polysimulator-extension: cancelled orphan order {vid}");
                host.note_orphan_cancelled(&vid).await;
            }
            Err(e) => {
                eprintln!("polysimulator-extension: failed to cancel orphan {vid}: {e}");
                host.report_error(CoreError::new(
                    CoreErrorCode::VenueError,
                    format!("orphan cancel failed for {vid}: {e}"),
                ))
                .await;
            }
        }
    }

    // Trades → reconcile snapshot; the core matches them to its own orders
    // and books what it owns. OUR fills are attributed through both leg
    // sides (taker + maker); rows that are not ours and rows we cannot read
    // are silent or logged, never forwarded as garbage.
    let mut matched = Vec::new();
    for row in &trades {
        if seen_trades.contains(&row.id) {
            continue;
        }
        // Mark seen regardless: an unreadable row must not be re-parsed every
        // sweep (it would spam the log forever).
        seen_trades.insert(row.id.clone());
        if let Some((vid, maker)) = our_leg(row, &known) {
            match to_venue_trade(row, vid, maker) {
                Some(trade) => matched.push(trade),
                None => eprintln!(
                    "polysimulator-extension: unreadable trade row {} (market {}) — skipped",
                    row.id, row.market
                ),
            }
        }
    }

    // FILLED-orders attribution (live venue 2026-10-26): the trade rows' leg
    // fields NEVER name our orders here — `taker_order_id` repeats the trade's
    // own id and `maker_orders` is absent — so the stream above cannot match a
    // single fill on this venue. The FILLED orders list does: each row names
    // its own order id, executed price and fill time. Every FILLED row whose
    // id the core knows becomes a synthetic reconcile trade; dedup key
    // `fill:{order_id}` (a limit order fills once at `fill_price`, so the
    // order id alone identifies the fill event).
    if let Ok(filled) = client.filled_orders().await {
        for o in &filled {
            let vid = o.venue_id();
            let key = format!("fill:{}", vid);
            if !known.contains(&vid) || seen_trades.contains(&key) {
                continue;
            }
            seen_trades.insert(key);
            let (Some(price), Some(size)) = (o.fill_price, o.quantity) else {
                eprintln!(
                    "polysimulator-extension: FILLED order {} lacks fill_price/quantity — skipped",
                    vid
                );
                continue;
            };
            if size <= Decimal::ZERO || price <= Decimal::ZERO || price >= Decimal::ONE {
                eprintln!(
                    "polysimulator-extension: FILLED order {} out-of-band price/size ({price}×{size}) — skipped",
                    vid
                );
                continue;
            }
            let ts_ms = o
                .filled_at
                .as_deref()
                .and_then(crate::discovery::iso_to_ms)
                .unwrap_or_else(now_ms);
            matched.push(VenueTradeInfo {
                venue_order_id: vid,
                trade_id: format!("fill:{}", o.venue_id()),
                token_id: o.market_id.clone(),
                side: match o.side.as_deref().map(str::to_ascii_uppercase).as_deref() {
                    Some("SELL") => Side::Sell,
                    _ => Side::Buy,
                },
                size,
                price,
                ts_ms,
                tx_hash: None,
                maker: Some(false),
            });
        }
    }
    let snapshot = ReconcileSnapshot {
        open_order_ids: open.iter().map(|o| o.venue_id()).collect(),
        trades: matched.clone(),
        now_ms: now_ms(),
    };
    host.on_reconcile(snapshot).await;
    for trade in &matched {
        host.on_fill(to_fill(&trade.venue_order_id, trade)).await;
    }

    // Cash truth: re-align the ledger's free view every sweep.
    match client.balance().await {
        Ok(b) => host.venue_free_balance("polysimulator", b.balance).await,
        Err(e) => host.report_error(into_core_error(&e)).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_place_body_prices_scale_to_1e6_integer_strings() {
        // BUY 5 shares at 0.435 → maker = 0.435·5 = 2.175 USDC = 2175000;
        // taker = 5 shares = 5000000. price = maker/taker = 0.435.
        let body = place_body_for(
            &Side::Buy,
            Decimal::from_str("0.435").unwrap(),
            Decimal::from(5),
            "tok-1",
        );
        assert_eq!(body.token_id, "tok-1");
        assert_eq!(body.side, "BUY");
        assert_eq!(body.maker_amount, "2175000");
        assert_eq!(body.taker_amount, "5000000");

        // SELL 5 shares at 0.90 → the pair inverts (price = taker/maker).
        let body = place_body_for(
            &Side::Sell,
            Decimal::from_str("0.90").unwrap(),
            Decimal::from(5),
            "tok-2",
        );
        assert_eq!(body.side, "SELL");
        assert_eq!(body.maker_amount, "5000000");
        assert_eq!(body.taker_amount, "4500000");
    }

    fn trade_row(
        id: &str,
        taker: Option<&str>,
        makers: Option<Vec<&str>>,
        side: &str,
        price: Option<&str>,
        asset: Option<&str>,
    ) -> crate::types::PmTradeRow {
        crate::types::PmTradeRow {
            id: id.to_string(),
            market: "0xabc".to_string(),
            asset_id: asset.map(|s| s.to_string()),
            side: Some(side.to_string()),
            outcome: Some("Yes".to_string()),
            price: price.map(|p| Decimal::from_str(p).unwrap()),
            size: Some(Decimal::from(10)),
            fee_rate_bps: Some(0),
            maker_orders: makers.map(|v| v.iter().map(|s| s.to_string()).collect()),
            taker_order_id: taker.map(|s| s.to_string()),
            transaction_hash: None,
            bucket_index: None,
            last_update: None,
            match_time: Some(1_773_583_200_000),
            owner: None,
            status: Some("MATCHED".to_string()),
            trader_side: None,
        }
    }
    use std::str::FromStr;

    #[test]
    fn trade_rows_attribute_through_both_leg_sides() {
        let our_ids: std::collections::HashSet<String> =
            ["41", "77"].iter().map(|s| s.to_string()).collect();
        // Taker leg is ours.
        let row = trade_row(
            "tx-1",
            Some("41"),
            Some(vec!["12"]),
            "BUY",
            Some("0.55"),
            Some("71321"),
        );
        assert_eq!(our_leg(&row, &our_ids), Some(("41".into(), false)));
        let trade = to_venue_trade(&row, "41".into(), false).expect("taker row maps");
        assert_eq!(trade.token_id, "71321");
        assert_eq!(trade.side, Side::Buy);
        assert_eq!(trade.price, Decimal::from_str("0.55").unwrap());
        assert_eq!(trade.maker, Some(false), "a taker-leg fill is not maker");

        // Maker leg is ours (our resting id inside maker_orders).
        let row_maker = trade_row(
            "tx-1",
            Some("9999"),
            Some(vec!["12", "77"]),
            "SELL",
            Some("0.55"),
            Some("71321"),
        );
        assert_eq!(our_leg(&row_maker, &our_ids), Some(("77".into(), true)));
        let trade = to_venue_trade(&row_maker, "77".into(), true).expect("maker row maps");
        assert_eq!(trade.side, Side::Sell);
        assert_eq!(trade.maker, Some(true), "a maker-leg fill is maker");

        // Neither leg is ours: not attributed.
        let row_other = trade_row(
            "tx-1",
            Some("9999"),
            Some(vec!["12"]),
            "BUY",
            Some("0.55"),
            Some("71321"),
        );
        assert_eq!(our_leg(&row_other, &our_ids), None);
    }

    #[test]
    fn unreadable_trade_rows_map_to_none_not_garbage() {
        let base = trade_row("tx-2", Some("41"), None, "BUY", None, Some("71321"));
        assert!(
            to_venue_trade(&base, "41".into(), false).is_none(),
            "no price"
        );

        let zero = trade_row("tx-2", Some("41"), None, "BUY", Some("0.55"), Some("71321"))
            .pipe_zero_size();
        assert!(
            to_venue_trade(&zero, "41".into(), false).is_none(),
            "zero size"
        );

        let out_of_band = trade_row("tx-2", Some("41"), None, "BUY", Some("1.0"), Some("71321"));
        assert!(
            to_venue_trade(&out_of_band, "41".into(), false).is_none(),
            "price 1.0 is not a tradeable probability"
        );

        let no_token = trade_row("tx-2", Some("41"), None, "BUY", Some("0.55"), None);
        assert!(
            to_venue_trade(&no_token, "41".into(), false).is_none(),
            "no asset_id"
        );
    }

    /// Build one updown row from its wire JSON (the same double-encoded
    /// nested strings the venue sends).
    fn updown_row(condition: &str, resolved: bool, up: &str, down: &str) -> UpDownRow {
        serde_json::from_str(&format!(
            r#"{{
                "event_id": "evt_1",
                "condition_id": "{condition}",
                "asset": "BTC",
                "interval": "15m",
                "active": true,
                "closed": {resolved},
                "resolved": {resolved},
                "markets": [{{
                    "condition_id": "{condition}",
                    "outcomes": "[\"Up\", \"Down\"]",
                    "outcome_prices": "[\"{up}\", \"{down}\"]",
                    "token_ids": "[\"tok-up\", \"tok-down\"]"
                }}]
            }}"#
        ))
        .expect("updown row parses")
    }

    fn query(condition: &str) -> SettlementQuery {
        SettlementQuery {
            condition_id: condition.to_string(),
            token_ids: vec!["tok-up".to_string(), "tok-down".to_string()],
            expires_at_ms: 0,
        }
    }

    #[test]
    fn settlement_answers_only_resolved_rows_with_terminal_payouts() {
        // Resolved row (up won): payouts 1/0 keyed by the parsed token ids.
        let rows = vec![updown_row("0xres", true, "1", "0")];
        let res = resolution_for(&query("0xres"), &rows).expect("resolved row answers");
        assert_eq!(res.condition_id, "0xres");
        assert!(res.resolved);
        assert_eq!(
            res.payouts,
            vec![
                ("tok-up".to_string(), Decimal::ONE),
                ("tok-down".to_string(), Decimal::ZERO)
            ]
        );
        assert_eq!(res.source, "polysimulator");
        assert!(!res.neg_risk);

        // Down won is symmetrical.
        let rows = vec![updown_row("0xdown", true, "0", "1")];
        let res = resolution_for(&query("0xdown"), &rows).expect("down-win answers");
        assert_eq!(
            res.payouts,
            vec![
                ("tok-up".to_string(), Decimal::ZERO),
                ("tok-down".to_string(), Decimal::ONE)
            ]
        );

        // Still live (resolved=false, prices mid-round): answer nothing —
        // the core re-queries on its own cadence.
        let rows = vec![updown_row("0xlive", false, "0.55", "0.45")];
        assert!(
            resolution_for(&query("0xlive"), &rows).is_none(),
            "unresolved row answers nothing"
        );

        // Resolved flag set but prices not yet terminal: not a verdict,
        // answer nothing (payouts are never guessed).
        let rows = vec![updown_row("0xhalf", true, "0.9", "0.1")];
        assert!(
            resolution_for(&query("0xhalf"), &rows).is_none(),
            "non-terminal prices answer nothing"
        );

        // A condition the updown list does not name: skip silently, never
        // fabricate.
        let rows = vec![updown_row("0xother", true, "1", "0")];
        assert!(
            resolution_for(&query("0xunknown"), &rows).is_none(),
            "unknown condition answers nothing"
        );
    }

    #[test]
    fn match_time_in_seconds_is_normalized_to_ms() {
        // Live venue (2026-10-26) reports match_time in SECONDS since epoch;
        // ts_ms consumers (incl. the kernel's external-fill watermark) expect
        // MILLISECONDS. A seconds-valued row must be scaled ×1000, and an
        // already-millisecond value must pass through untouched.
        let mut row = trade_row("tx-3", Some("41"), None, "BUY", Some("0.55"), Some("71321"));

        row.match_time = Some(1_792_995_878); // seconds: 2026-10-26T06:24:38Z
        let trade = to_venue_trade(&row, "41".into(), false).expect("seconds row maps");
        assert_eq!(trade.ts_ms, 1_792_995_878_000, "seconds scaled to ms");

        row.match_time = Some(1_773_583_200_000); // already ms
        let trade = to_venue_trade(&row, "41".into(), false).expect("ms row maps");
        assert_eq!(trade.ts_ms, 1_773_583_200_000, "ms passes through");

        row.match_time = None;
        let now = now_ms();
        let trade = to_venue_trade(&row, "41".into(), false).expect("absent row maps");
        assert!(trade.ts_ms >= now, "absent match_time falls back to now_ms");
    }

    /// Test helper: rebuild a row with a zero size (struct has no builder).
    trait ZeroSize {
        fn pipe_zero_size(self) -> crate::types::PmTradeRow;
    }
    impl ZeroSize for crate::types::PmTradeRow {
        fn pipe_zero_size(mut self) -> crate::types::PmTradeRow {
            self.size = Some(Decimal::ZERO);
            self
        }
    }
}
