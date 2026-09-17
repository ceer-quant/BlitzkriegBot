//! Live CLOB venue — actor wrapping the authenticated Polymarket Rust SDK.
//!
//! The authenticated SDK client type is not nameable from outside the SDK, so the
//! client lives inside a spawned actor task (type inference owns it) and the rest
//! of the system talks to it through a typed command channel. One task owns REST +
//! user-WS and feeds fills back to the host.
//!
//! Credentials are read here from the process environment; Node never supplies
//! them. The venue maps SDK types onto the market-api boundary types.

use alloy::signers::Signer as _;
use alloy::signers::local::LocalSigner;
use anyhow::Context as _;
use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, FillPolicy, MarketFill, PendingOrder, Side,
    VenueTradeInfo,
};
use futures::StreamExt;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::{Credentials, Kind, Normal};
use polymarket_client_sdk_v2::clob::types::request::{
    BalanceAllowanceRequest, OrdersRequest, TradesRequest,
};
use polymarket_client_sdk_v2::clob::types::{
    AssetType, OrderType as SdkOrderType, Side as SdkSide, SignatureType,
};
use polymarket_client_sdk_v2::clob::ws::Client as WsClient;
use polymarket_client_sdk_v2::clob::ws::types::response::{
    TradeMessage, TradeMessageStatus, WsMessage,
};
use polymarket_client_sdk_v2::clob::{Client, Config};
use polymarket_client_sdk_v2::types::{Address, B256, Decimal as SdkDecimal, U256};
use polymarket_client_sdk_v2::ws::config::Config as WsConfig;
use rust_decimal::Decimal;
use std::str::FromStr;
use tokio::sync::{mpsc, oneshot};

const DEFAULT_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com";

// ── Handle ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VenueCmd {
    Place {
        order: PendingOrder,
        reply: oneshot::Sender<CoreResult<VenuePlace>>,
    },
    Cancel {
        venue_order_id: String,
        reply: oneshot::Sender<CoreResult<()>>,
    },
    Balance {
        reply: oneshot::Sender<CoreResult<Decimal>>,
    },
    /// Open order ids + recent trades for the reconciliation sweep.
    Snapshot {
        reply: oneshot::Sender<CoreResult<(Vec<String>, Vec<VenueTradeInfo>)>>,
    },
}

/// Result of posting an order to the venue.
#[derive(Debug, Clone)]
pub struct VenuePlace {
    pub venue_order_id: String,
    pub success: bool,
    pub status: String,
}

/// Handle to the running venue actor.
#[derive(Debug, Clone)]
pub struct LiveVenue {
    tx: mpsc::Sender<VenueCmd>,
    pub signer: String,
    pub funder: String,
}

impl LiveVenue {
    pub async fn place(&self, order: PendingOrder) -> CoreResult<VenuePlace> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VenueCmd::Place { order, reply: tx })
            .await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor stopped"))?;
        rx.await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor dropped reply"))?
    }

    pub async fn cancel(&self, venue_order_id: String) -> CoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VenueCmd::Cancel {
                venue_order_id,
                reply: tx,
            })
            .await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor stopped"))?;
        rx.await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor dropped reply"))?
    }

    pub async fn balance(&self) -> CoreResult<Decimal> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VenueCmd::Balance { reply: tx })
            .await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor stopped"))?;
        rx.await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor dropped reply"))?
    }

    pub async fn snapshot(&self) -> CoreResult<(Vec<String>, Vec<VenueTradeInfo>)> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VenueCmd::Snapshot { reply: tx })
            .await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor stopped"))?;
        rx.await
            .map_err(|_| CoreError::new(CoreErrorCode::Internal, "venue actor dropped reply"))?
    }
}

/// A host-destined event produced by the venue (user-WS stream).
#[derive(Debug, Clone)]
pub enum VenueEvent {
    /// Authoritative fill for one of our orders (per-execution, OME dedups).
    Fill(MarketFill),
    /// Venue confirmed an order is resting (PLACEMENT/UPDATE).
    OrderLive {
        venue_order_id: String,
    },
    /// Venue reports the order cancelled/resting-size reduced.
    OrderCancelled {
        venue_order_id: String,
    },
    /// Human-readable reconcile hint (currently informational only).
    ReconcileReport(String),
    Fatal(String),
}

// ── Actor ───────────────────────────────────────────────────────────────────

/// Spawn the venue actor. Returns the handle and a receiver for host-destined
/// events (fills / order confirmations / reconcile reports).
pub async fn spawn_from_env(
    markets: Vec<String>,
    events: mpsc::Sender<VenueEvent>,
) -> anyhow::Result<LiveVenue> {
    let pk = std::env::var("POLYMARKET_PRIVATE_KEY")
        .context("POLYMARKET_PRIVATE_KEY required for live mode")?;
    let funder_str =
        std::env::var("POLYMARKET_FUNDER_ADDRESS").context("POLYMARKET_FUNDER_ADDRESS required")?;
    let url =
        std::env::var("CLOB_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".into());
    let ws_url = std::env::var("POLYMARKET_WS_URL").unwrap_or_else(|_| DEFAULT_WS_URL.into());

    let funder = Address::from_str(&funder_str)?;
    let signer = LocalSigner::from_str(&pk)?.with_chain_id(Some(polymarket_client_sdk_v2::POLYGON));
    let signer_address = signer.address();

    let client_unauth = Client::new(&url, Config::builder().use_server_time(true).build())?;
    // L2 credentials for the user WebSocket, derived from the private key here
    // so Node never needs API keys at all.
    let ws_credentials: Credentials = client_unauth
        .create_or_derive_api_key(&signer, None)
        .await?;
    let client = client_unauth
        .authentication_builder(&signer)
        .funder(funder)
        .signature_type(SignatureType::Poly1271)
        .authenticate()
        .await?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<VenueCmd>(64);
    let handle = LiveVenue {
        tx: cmd_tx,
        signer: signer_address.to_string(),
        funder: funder_str.clone(),
    };

    tokio::spawn(async move {
        actor_loop(
            client,
            signer,
            funder,
            ws_url,
            markets,
            ws_credentials,
            cmd_rx,
            events,
        )
        .await;
    });

    Ok(handle)
}

async fn actor_loop<S: alloy::signers::Signer + Clone + Send + Sync + 'static>(
    client: Client<Authenticated<Normal>>,
    signer: S,
    funder: Address,
    ws_url: String,
    markets: Vec<String>,
    ws_credentials: Credentials,
    mut cmd_rx: mpsc::Receiver<VenueCmd>,
    events: mpsc::Sender<VenueEvent>,
) {
    // User-WS stream (best effort: skip when no markets are provided).
    if !markets.is_empty() {
        if let Err(e) = start_user_ws(funder, ws_url, markets, ws_credentials, events.clone()).await
        {
            let _ = events
                .send(VenueEvent::Fatal(format!("user ws setup failed: {e}")))
                .await;
        }
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            VenueCmd::Place { order, reply } => {
                let res = sdk_place(&client, &signer, &order).await;
                let _ = reply.send(res);
            }
            VenueCmd::Cancel {
                venue_order_id,
                reply,
            } => {
                let res = client
                    .cancel_order(&venue_order_id)
                    .await
                    .map(|_| ())
                    .map_err(|e| map_sdk_err(&e.to_string()));
                let _ = reply.send(res);
            }
            VenueCmd::Balance { reply } => {
                let res = sdk_balance(&client).await;
                let _ = reply.send(res);
            }
            VenueCmd::Snapshot { reply } => {
                let res = sdk_snapshot(&client).await;
                let _ = reply.send(res);
            }
        }
    }
}

async fn sdk_place<S: alloy::signers::Signer + Sync>(
    client: &Client<Authenticated<Normal>>,
    signer: &S,
    order: &PendingOrder,
) -> CoreResult<VenuePlace> {
    let token = U256::from_str(&order.token_id)
        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, format!("bad token id: {e}")))?;
    let price = SdkDecimal::from_str(&order.price.to_string())
        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, format!("bad price: {e}")))?;
    let size = SdkDecimal::from_str(&order.size.to_string())
        .map_err(|e| CoreError::new(CoreErrorCode::InvalidParams, format!("bad size: {e}")))?;
    let side = match order.side {
        Side::Buy => SdkSide::Buy,
        Side::Sell => SdkSide::Sell,
    };
    let (order_type, post_only) = match order.fill_policy {
        FillPolicy::Taker => (SdkOrderType::FOK, false),
        FillPolicy::Maker | FillPolicy::MakerThenTaker => (SdkOrderType::GTC, true),
    };

    let resp = client
        .limit_order()
        .token_id(token)
        .side(side)
        .price(price)
        .size(size)
        .order_type(order_type)
        .post_only(post_only)
        .build_sign_and_post(signer)
        .await
        .map_err(|e| map_sdk_err(&e.to_string()))?;

    if !resp.success {
        return Err(
            CoreError::new(CoreErrorCode::VenueError, "venue rejected order")
                .with_raw(resp.error_msg.unwrap_or_default()),
        );
    }
    Ok(VenuePlace {
        venue_order_id: resp.order_id,
        success: resp.success,
        status: format!("{:?}", resp.status),
    })
}

async fn sdk_balance(client: &Client<Authenticated<Normal>>) -> CoreResult<Decimal> {
    let req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    let bal = client
        .balance_allowance(req)
        .await
        .map_err(|e| map_sdk_err(&e.to_string()))?;
    Decimal::from_str(&format!("{}", bal.balance))
        .map_err(|e| CoreError::new(CoreErrorCode::VenueError, format!("balance parse: {e}")))
}

async fn sdk_snapshot(
    client: &Client<Authenticated<Normal>>,
) -> CoreResult<(Vec<String>, Vec<VenueTradeInfo>)> {
    let open = client
        .orders(&OrdersRequest::builder().build(), None)
        .await
        .map_err(|e| map_sdk_err(&e.to_string()))?;
    let open_ids = open.data.into_iter().map(|o| o.id).collect();

    let page = client
        .trades(&TradesRequest::builder().build(), None)
        .await
        .map_err(|e| map_sdk_err(&e.to_string()))?;
    let trades = page
        .data
        .into_iter()
        .map(|t| VenueTradeInfo {
            venue_order_id: t.taker_order_id.clone(),
            trade_id: t.id,
            token_id: format!("{}", t.asset_id),
            side: map_side(t.side),
            size: t.size,
            price: t.price,
            ts_ms: t.match_time.timestamp_millis(),
            tx_hash: None,
            // Indexed by the venue's taker order id, so this record IS the taker
            // execution. Maker executions come through the user channel, where
            // their `maker_orders[]` entry is tagged maker.
            maker: Some(false),
        })
        .collect();
    Ok((open_ids, trades))
}

/// Subscribe the authenticated user channel and translate SDK messages into
/// venue events. The stream ends only on error; a real deployment should
/// reconnect with backoff (the actor keeps accepting commands meanwhile).
async fn start_user_ws(
    funder: Address,
    ws_url: String,
    markets: Vec<String>,
    ws_credentials: Credentials,
    events: mpsc::Sender<VenueEvent>,
) -> anyhow::Result<()> {
    let ws = WsClient::new(&ws_url, WsConfig::default())?.authenticate(ws_credentials, funder)?;
    let condition_ids: Vec<B256> = markets
        .iter()
        .filter_map(|m| B256::from_str(m).ok())
        .collect();
    if condition_ids.is_empty() {
        anyhow::bail!("no valid market condition ids for user ws");
    }
    let stream = ws.subscribe_user_events(condition_ids)?;
    tokio::spawn(async move {
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(WsMessage::Trade(t)) => {
                    for fill in trade_fills(&t) {
                        if events.send(VenueEvent::Fill(fill)).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(WsMessage::Order(o)) => {
                    let ev = match o.msg_type {
                        Some(polymarket_client_sdk_v2::clob::ws::types::response::OrderMessageType::Cancellation) => {
                            VenueEvent::OrderCancelled { venue_order_id: o.id.clone() }
                        }
                        _ => VenueEvent::OrderLive { venue_order_id: o.id.clone() },
                    };
                    if events.send(ev).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = events
                        .send(VenueEvent::Fatal(format!("user ws error: {e}")))
                        .await;
                    break;
                }
            }
        }
    });
    Ok(())
}

/// Map a user trade message onto per-order fills. Our orders may appear as the
/// taker (`taker_order_id`) or as makers (`maker_orders[]`).
fn trade_fills(t: &TradeMessage) -> Vec<MarketFill> {
    let status = match t.status {
        TradeMessageStatus::Confirmed => blitzkrieg_market_api::FillStatus::Confirmed,
        TradeMessageStatus::Failed => blitzkrieg_market_api::FillStatus::Failed,
        _ => blitzkrieg_market_api::FillStatus::Matched,
    };
    let ts = t.timestamp.unwrap_or(0);
    let tx = t.transaction_hash.as_ref().map(|h| format!("{h}"));
    let mut fills = Vec::new();

    if let Some(taker_id) = &t.taker_order_id {
        fills.push(MarketFill {
            order_id: taker_id.clone(),
            trade_id: Some(t.id.clone()),
            token_id: format!("{}", t.asset_id),
            side: map_side(t.side),
            price: t.price,
            size: t.size,
            status,
            ts_ms: ts,
            tx_hash: tx.clone(),
            // We crossed: our order is the one the venue names as the taker.
            maker: Some(false),
        });
    }
    for m in &t.maker_orders {
        fills.push(MarketFill {
            order_id: m.order_id.clone(),
            trade_id: Some(format!("{}:{}", t.id, m.order_id)),
            token_id: format!("{}", m.asset_id),
            side: map_side(t.side).invert(),
            price: m.price,
            size: m.matched_amount,
            status,
            ts_ms: ts,
            tx_hash: tx.clone(),
            // We rested and were hit, so no taker fee was charged.
            maker: Some(true),
        });
    }
    fills
}

fn map_side(s: SdkSide) -> Side {
    match s {
        SdkSide::Buy => Side::Buy,
        _ => Side::Sell,
    }
}

/// Map venue/SDK error text to a structured code without losing the raw text.
pub fn map_sdk_err(raw: &str) -> CoreError {
    let lower = raw.to_lowercase();
    let code = if lower.contains("insufficient")
        || lower.contains("allowance")
        || lower.contains("balance")
    {
        CoreErrorCode::InsufficientFunds
    } else if lower.contains("tick") {
        CoreErrorCode::InvalidTickSize
    } else if lower.contains("match") || lower.contains("cross") || lower.contains("post-only") {
        CoreErrorCode::WouldCross
    } else if lower.contains("halt") || lower.contains("closed") {
        CoreErrorCode::MarketHalted
    } else if lower.contains("auth") {
        CoreErrorCode::NotAuthenticated
    } else if lower.contains("timeout") {
        CoreErrorCode::Timeout
    } else {
        CoreErrorCode::VenueError
    };
    CoreError::new(code, "venue request failed").with_raw(raw)
}

// Keep Kind referenced so the generic bound stays documented for live wiring.
#[allow(dead_code)]
fn _kind_is_used<K: Kind>(_: &K) {}
