//! The order executor: the market-plugin component that turns the core's
//! locally-accepted orders into signed predict.fun placements and reconciles
//! the venue's own state back (stage-4 completion — the component #423/#424
//! left an "arrives with stage 4" note about; #483).
//!
//! predict.fun has no private WebSocket push and no fills/trades endpoint,
//! so the executor is a REST polling actor on the same shape as the sibling
//! executors: one tokio task owns a credentialed [`PredictRest`], drains the
//! host's pending orders / cancels every tick, and runs a periodic
//! reconciliation sweep (open orders + fill watermarks + orphan cleanup +
//! free-balance sync). Everything the core sees crosses the `MarketHost`
//! seam; all predict.fun knowledge stays here.
//!
//! predict.fun specifics at the boundary:
//!   * placement — an EIP-712 signed order ([`crate::signing`]). The
//!     signature determines the venue-side order hash, so a placement is
//!     signed ONCE and the transport's retry resends it byte-identically
//!     (the venue dedupes by hash). A timeout does NOT imply rejection: the
//!     (hash → core id) pair stays in flight and the sweep adopts the order
//!     when the hash shows up in the venue's order list.
//!   * fills — the venue reports a cumulative `amountFilled` watermark on
//!     its order rows and nothing else. The executor diffs the watermark
//!     per venue id; the delta is a fill with the deterministic dedup key
//!     `{venue_id}:{cumulative}`. Price is the order's limit (approximation,
//!     disclosed — the venue publishes no per-match prints).
//!   * identity — the market row (which selects the exchange contract via
//!     `is_neg_risk × is_yield_bearing`, and carries the fee) is fetched per
//!     token and cached; a token the venue does not know is refused, never
//!     guessed.
//!
//! Credentials (environment only): `PREDICT_API_KEY` +
//! `PREDICT_PRIVATE_KEY` (EOA hex key). With both present the executor also
//! mints its own JWT when `PREDICT_JWT_TOKEN` is absent (stage 1's
//! `/v1/auth/message` + EIP-191 `/v1/auth` exchange). Either missing → the
//! executor is INERT (`Ok(None)`; dry runs keep an honest `has_executor`
//! with zero egress).

use crate::error::PredictError;
use crate::into_core_error;
use crate::rest::{Credentials, PlacedOrder, PredictRest, RestConfig};
use crate::signing::{OrderSigner, exchange_address};
use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, ExecutorConfig, FillStatus, MarketFill, MarketHost,
    PendingOrder, ReconcileSnapshot, SelfCheckItem, SelfCheckReport, Side, VenueTradeInfo,
    net::now_ms,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
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

/// Build the executor's inputs from the environment, or `None` when either
/// credential is absent — the executor is then INERT (dry runs). Same
/// contract as the sibling executors' `spawn_if_configured`. A JWT from the
/// environment rides along; its absence just means the loop mints one from
/// the wallet at startup.
fn credentials_from_env() -> Option<(Credentials, String)> {
    let api_key = std::env::var("PREDICT_API_KEY").ok()?;
    let private_key = std::env::var("PREDICT_PRIVATE_KEY").ok()?;
    if api_key.is_empty() || private_key.is_empty() {
        return None;
    }
    let jwt = std::env::var("PREDICT_JWT_TOKEN")
        .ok()
        .filter(|j| !j.is_empty());
    Some((
        Credentials {
            api_key: Some(api_key),
            jwt,
        },
        private_key,
    ))
}

fn rest_config() -> RestConfig {
    let mut config = RestConfig::default();
    if std::env::var("PREDICT_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    config
}

/// Mint a JWT with the wallet when the client carries none: fetch the
/// venue's signing message, EIP-191-sign it, exchange for a token. Stage 1's
/// `auth_message`/`exchange_jwt` finally get a production caller. A mint
/// failure is survivable (auth may recover on the venue side); every order
/// placement will fail with the venue's auth error until it succeeds.
async fn ensure_jwt(client: &PredictRest, signer: &OrderSigner) {
    if client.has_jwt().await {
        return;
    }
    match try_mint_jwt(client, signer).await {
        Ok(jwt) => {
            client.set_jwt(jwt).await;
            eprintln!("predictfun-extension: JWT minted from wallet");
        }
        Err(e) => eprintln!(
            "predictfun-extension: JWT mint failed (order placement will fail until auth works): {e}"
        ),
    }
}

async fn try_mint_jwt(client: &PredictRest, signer: &OrderSigner) -> Result<String, PredictError> {
    let message = client.auth_message().await?;
    // EIP-191 personal_sign: the signer signs the message bytes directly.
    let signature = signer.sign_personal_message(message.as_bytes()).await?;
    client
        .exchange_jwt(&format!("{:#x}", signer.address), &signature, &message)
        .await
}

/// The cached identity of one token: which exchange contract settles it and
/// what the venue charges. A token the venue does not know is refused.
#[derive(Debug, Clone)]
struct TokenIdentity {
    verifying_contract: &'static str,
    fee_rate_bps: u64,
}

/// Spawn the executor. Inert (Ok(None)) without credentials; otherwise a
/// background task runs until the process ends.
pub async fn spawn_if_configured(
    host: Arc<dyn MarketHost>,
    _config: ExecutorConfig,
) -> CoreResult<Option<tokio::task::JoinHandle<()>>> {
    let Some((credentials, private_key)) = credentials_from_env() else {
        return Ok(None);
    };
    let signer = OrderSigner::from_hex_key(&private_key).map_err(|e| {
        CoreError::new(
            CoreErrorCode::InvalidParams,
            format!("predictfun signer: {e}"),
        )
    })?;
    let client = Arc::new(
        PredictRest::new(rest_config(), Some(credentials)).map_err(|e| {
            CoreError::new(CoreErrorCode::VenueError, format!("predictfun client: {e}"))
        })?,
    );
    let handle = tokio::spawn(executor_loop(host, client, signer));
    Ok(Some(handle))
}

/// Test-only entry for the executor integration suite: drive the REAL
/// production loop against a mock server with an explicit client (env-var
/// plumbing stays out of the test's control, and no credential is fixed in
/// source). Not part of the public surface, exactly like the feed's
/// `feed_tests_only_spawn`.
#[doc(hidden)]
pub fn executor_tests_only_spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<PredictRest>,
    signer: OrderSigner,
) {
    tokio::spawn(executor_loop(host, client, signer));
}

/// One capability probe against the paths trading actually uses: balance
/// (auth) and the portfolio order list (the sweep transport). A failed item
/// means order egress cannot work either.
async fn self_check(client: &PredictRest) -> SelfCheckReport {
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
    ];
    SelfCheckReport {
        ok: items.iter().all(|i| i.ok),
        ts_ms: now_ms(),
        items,
    }
}

/// Per-order executor memory: what we believe about a venue order. Keyed by
/// the EIP-712 order hash — the only stable id across the venue's db-id and
/// hash renames; the core-side mapping was already reported at accept time.
#[derive(Debug, Clone)]
struct Tracked {
    /// Cumulative filled shares last seen on the venue row.
    filled_so_far: Decimal,
}

pub(crate) async fn executor_loop(
    host: Arc<dyn MarketHost>,
    client: Arc<PredictRest>,
    signer: OrderSigner,
) {
    let mut tick = interval(Duration::from_millis(EXECUTOR_TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut since_sweep: u64 = 0;
    let mut sweep_failures: u32 = 0;
    let mut place_failures: u32 = 0;
    // (order hash → tracked state) for everything we signed; the venue's
    // order hash is the only stable key across its id renames.
    let mut in_flight: HashMap<String, Tracked> = HashMap::new();
    // Orders just placed, still absent from the venue's order list (the
    // orphan sweep's grace window).
    let mut recent_placements: Vec<(String, i64)> = Vec::new();
    const ORPHAN_GRACE_MS: i64 = 30_000;
    // Token → (contract, fee) cache; the market row also routes identity.
    let mut token_idsentities: HashMap<String, TokenIdentity> = HashMap::new();

    // Auth: the operator's JWT when the environment handed one over, else
    // mint one from the wallet.
    ensure_jwt(&client, &signer).await;

    // Seed the ledger from the venue so the reserve gate reflects real cash.
    match client.balance().await {
        Ok(b) => {
            host.seed_balance("predictfun", b.balance).await;
            eprintln!("predictfun-extension: executor ready balance={}", b.balance);
        }
        Err(e) => eprintln!("predictfun-extension: live balance fetch failed: {e}"),
    }
    let report = self_check(&client).await;
    if !report.ok {
        eprintln!("predictfun-extension: startup self-check FAILED");
    }
    host.on_self_check(report).await;

    // Startup orphan sweep: anything resting at the venue the core does not
    // know about is cancelled before trading resumes. (At startup nothing is
    // in flight, so every unknown open order is an orphan.)
    startup_orphan_sweep(&host, &client).await;

    loop {
        tick.tick().await;

        // 1) Cancels first (same serialization rule as the sibling executors:
        //    the old order is gone before a replacement is submitted).
        let mut cancel_strikes = 0u32;
        for id in host.take_pending_cancels("predictfun").await {
            match client.cancel_orders(&[&id]).await {
                Ok(_) => host.note_orphan_cancelled(&id).await,
                Err(e) => {
                    cancel_strikes += 1;
                    eprintln!("predictfun-extension: venue cancel failed for {id}: {e}");
                    host.report_error(into_core_error(&e)).await;
                }
            }
        }

        // 2) Submit locally-accepted orders. `take_pending_orders` routes by
        //    this plugin's venue name — with multiple executors running, each
        //    drains only ITS venue's orders.
        if cancel_strikes == 0 {
            for order in host.take_pending_orders("predictfun").await {
                match place(&client, &signer, &order, &mut token_idsentities).await {
                    Ok((placed, order_hash)) => {
                        place_failures = 0;
                        recent_placements.push((placed.venue_id.clone(), now_ms()));
                        in_flight.insert(
                            order_hash,
                            Tracked {
                                filled_so_far: Decimal::ZERO,
                            },
                        );
                        host.on_order_accepted(&order.core_order_id, &placed.venue_id)
                            .await;
                    }
                    Err(e) if e.code == CoreErrorCode::Timeout => {
                        place_failures += 1;
                        host.on_place_timeout(&order.core_order_id).await;
                        host.report_error(e).await;
                    }
                    Err(e) => {
                        place_failures += 1;
                        eprintln!(
                            "predictfun-extension: place failed for {}: {e}",
                            order.core_order_id
                        );
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
            &mut in_flight,
            &mut recent_placements,
            ORPHAN_GRACE_MS,
        )
        .await
        {
            Ok(()) => sweep_failures = 0,
            Err(e) => {
                sweep_failures += 1;
                eprintln!("predictfun-extension: sweep failed ({sweep_failures}): {e}");
                if sweep_failures >= SWEEP_FAILURE_FREEZE {
                    host.on_reconcile_failed(e).await;
                } else {
                    host.report_error(e).await;
                }
            }
        }
    }
}

/// Resolve a token's contract+fee, via the cache, from the venue's own market
/// row. A token the venue does not know is refused — never guessed.
async fn token_identity(
    client: &PredictRest,
    cache: &mut HashMap<String, TokenIdentity>,
    token_id: &str,
) -> CoreResult<TokenIdentity> {
    if let Some(hit) = cache.get(token_id) {
        return Ok(hit.clone());
    }
    let market = client
        .market(token_id)
        .await
        .map_err(|e| into_core_error(&e))?;
    let identity = TokenIdentity {
        verifying_contract: exchange_address(market.is_neg_risk, market.is_yield_bearing),
        fee_rate_bps: market.fee_rate_bps,
    };
    cache.insert(token_id.to_string(), identity.clone());
    Ok(identity)
}

/// Submit one order: quantize, EIP-712 sign, POST. Returns the venue's
/// placement ack plus the order hash (the reconcile key). A timeout here
/// does NOT mean the order is dead — the hash stays in flight and the sweep
/// adopts or retires it.
async fn place(
    client: &PredictRest,
    signer: &OrderSigner,
    order: &PendingOrder,
    cache: &mut HashMap<String, TokenIdentity>,
) -> CoreResult<(PlacedOrder, String)> {
    let identity = token_identity(client, cache, &order.token_id).await?;
    let side_buy = order.side == Side::Buy;
    let (body, order_hash) = signer
        .sign_limit_order(
            &order.token_id,
            order.price,
            order.size,
            side_buy,
            identity.fee_rate_bps,
            identity.verifying_contract,
        )
        .await
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("order construction: {e}"),
            )
        })?;
    let placed = client
        .place_order(body)
        .await
        .map_err(|e| into_core_error(&e))?;
    Ok((placed, order_hash))
}

/// Startup variant of the orphan sweep: nothing is in flight yet, so every
/// venue-open order the core has never heard of is cancelled.
async fn startup_orphan_sweep(host: &Arc<dyn MarketHost>, client: &PredictRest) {
    match client.orders(None).await {
        Ok(open) => {
            let known: std::collections::HashSet<String> =
                host.known_venue_order_ids().await.into_iter().collect();
            for o in &open {
                let Some(id) = venue_order_id(o) else {
                    continue;
                };
                if known.contains(&id) {
                    continue;
                }
                if let Err(e) = client.cancel_orders(&[&id]).await {
                    eprintln!("predictfun-extension: orphan cancel {id}: {e}");
                } else {
                    host.note_orphan_cancelled(&id).await;
                }
            }
        }
        Err(e) => eprintln!("predictfun-extension: startup orphan sweep failed: {e}"),
    }
}

/// The venue id of an order row: db id first (the cancel key), hash second.
fn venue_order_id(o: &crate::types::PredictOrder) -> Option<String> {
    o.id.clone().or_else(|| o.order_hash.clone())
}

/// One reconciliation pass: open orders + fill watermarks + orphan cleanup +
/// free balance. Any transport failure aborts the pass so the failure counter
/// sees a coherent "sweep did not complete", not a half-swept ledger.
async fn sweep(
    host: &Arc<dyn MarketHost>,
    client: &PredictRest,
    in_flight: &mut HashMap<String, Tracked>,
    recent_placements: &mut Vec<(String, i64)>,
    orphan_grace_ms: i64,
) -> CoreResult<()> {
    let open = client.orders(None).await.map_err(|e| into_core_error(&e))?;

    // Adopt / track / fill-diff over the venue's rows. A row whose hash we
    // signed is ours whether or not its ack ever reached us (timeout case);
    // its cumulative `amountFilled` watermark yields per-sweep fill deltas.
    let mut trades: Vec<VenueTradeInfo> = Vec::new();
    let mut open_ids: Vec<String> = Vec::new();
    for o in &open {
        let Some(row_id) = venue_order_id(o) else {
            continue;
        };
        if !o.is_terminal() {
            open_ids.push(row_id.clone());
        }
        let Some(hash) = o.order_hash.clone() else {
            continue;
        };
        let Some(tracked) = in_flight.get_mut(&hash) else {
            continue;
        };
        let cumulative = o.amount_filled.unwrap_or(Decimal::ZERO);
        if cumulative > tracked.filled_so_far {
            let delta = cumulative - tracked.filled_so_far;
            tracked.filled_so_far = cumulative;
            if delta > Decimal::ZERO && o.price > Decimal::ZERO {
                trades.push(VenueTradeInfo {
                    venue_order_id: row_id.clone(),
                    // Deterministic dedup key: id + the watermark it was seen at.
                    trade_id: format!("{row_id}:{cumulative}"),
                    token_id: o.token_id.clone(),
                    side: if o.side.as_deref() == Some("SELL") {
                        Side::Sell
                    } else {
                        Side::Buy
                    },
                    size: delta,
                    price: o.price,
                    ts_ms: o.timestamp.unwrap_or_else(now_ms),
                    tx_hash: None,
                    maker: None,
                });
            }
        }
    }

    // Orphan sweep: a venue-open id the core has never heard of gets cancelled
    // (a POST accepted after our timeout — adopted above by hash — or a
    // resting order whose cancel failed). Fresh placements ride out a grace
    // window. Known = adopted in-flight ids + the host's own + fresh ones.
    let known: std::collections::HashSet<String> = {
        let mut known: std::collections::HashSet<String> =
            host.known_venue_order_ids().await.into_iter().collect();
        known.extend(open.iter().filter_map(|o| {
            o.order_hash
                .as_deref()
                .and_then(|h| in_flight.get(h))
                .and_then(|_| venue_order_id(o))
        }));
        known
    };
    let now = now_ms();
    recent_placements.retain(|(_, at)| now - *at < orphan_grace_ms);
    for o in &open {
        let Some(row_id) = venue_order_id(o) else {
            continue;
        };
        if o.is_terminal()
            || known.contains(&row_id)
            || recent_placements.iter().any(|(rid, _)| *rid == row_id)
        {
            continue;
        }
        match client.cancel_orders(&[&row_id]).await {
            Ok(_) => {
                eprintln!("predictfun-extension: cancelled orphan order {row_id}");
                host.note_orphan_cancelled(&row_id).await;
            }
            Err(e) => {
                eprintln!("predictfun-extension: failed to cancel orphan {row_id}: {e}");
                host.report_error(CoreError::new(
                    CoreErrorCode::VenueError,
                    format!("orphan cancel failed for {row_id}: {e}"),
                ))
                .await;
            }
        }
    }

    let snapshot = ReconcileSnapshot {
        open_order_ids: open_ids,
        trades: trades.clone(),
        now_ms: now_ms(),
    };
    host.on_reconcile(snapshot).await;
    for t in &trades {
        host.on_fill(MarketFill {
            order_id: t.venue_order_id.clone(),
            trade_id: Some(t.trade_id.clone()),
            token_id: t.token_id.clone(),
            side: t.side,
            price: t.price,
            size: t.size,
            status: FillStatus::Matched,
            ts_ms: t.ts_ms,
            tx_hash: None,
            maker: t.maker,
        })
        .await;
    }

    // Cash truth: re-align the ledger's free view every sweep.
    match client.balance().await {
        Ok(b) => host.venue_free_balance("predictfun", b.balance).await,
        Err(e) => host.report_error(into_core_error(&e)).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_order_id_prefers_the_db_id() {
        let mut o = order_row();
        o.id = Some("db-1".into());
        o.order_hash = Some("0xhash".into());
        assert_eq!(venue_order_id(&o).as_deref(), Some("db-1"));
        o.id = None;
        assert_eq!(venue_order_id(&o).as_deref(), Some("0xhash"));
        o.order_hash = None;
        assert_eq!(venue_order_id(&o), None);
    }

    fn order_row() -> crate::types::PredictOrder {
        crate::types::PredictOrder {
            id: None,
            order_hash: None,
            token_id: "0xtok".into(),
            side: Some("BUY".into()),
            order_type: None,
            price: Decimal::from_str_exact("0.44").unwrap(),
            shares: Decimal::from(10),
            status: Some("OPEN".into()),
            timestamp: Some(1_000),
            amount_filled: None,
        }
    }
}
