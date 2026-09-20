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
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use blitzkrieg_market_api::{
    CoreError, CoreErrorCode, CoreResult, FillPolicy, MarketFill, PendingOrder, SelfCheckItem,
    SelfCheckReport, Side, VenueTradeInfo,
};
use futures_util::StreamExt;
use hmac::{Hmac, Mac as _};
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::{Credentials, ExposeSecret, Kind, Normal};
use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk_v2::clob::types::{
    AssetType, OrderType as SdkOrderType, Side as SdkSide, SignatureType,
};
use polymarket_client_sdk_v2::clob::ws::Client as WsClient;
use polymarket_client_sdk_v2::clob::ws::types::response::{
    TradeMessage, TradeMessageStatus, WsMessage,
};
use polymarket_client_sdk_v2::clob::{Client, Config};
use polymarket_client_sdk_v2::types::{Address, Decimal as SdkDecimal, U256};
use polymarket_client_sdk_v2::ws::config::Config as WsConfig;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE};
use rust_decimal::Decimal;
use sha2::Sha256;
use std::str::FromStr;
use tokio::sync::{mpsc, oneshot};

const DEFAULT_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com";

/// The sweep transport impersonates a browser: Cloudflare fronting the CLOB
/// treats unknown custom UAs differently per endpoint class, and a
/// plain-looking client keeps the read-only sweep out of bot-rule drift.
const SWEEP_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// The venue URL is process-local config (CLOB_API_URL). Only http(s) with a
/// public, non-reserved host is acceptable — no loopback, private-range or
/// reserved-address destinations.
fn validate_venue_host(raw: &str) -> anyhow::Result<String> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("CLOB_API_URL must be http(s), got: {raw}"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_part = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if host_part.starts_with('[') {
        host_part.trim_matches(['[', ']']).to_ascii_lowercase()
    } else {
        host_part
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    anyhow::ensure!(!host.is_empty(), "CLOB_API_URL has no host");

    let reserved_name = host == "localhost"
        || host == "local"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".internal.invalid");
    anyhow::ensure!(
        !reserved_name,
        "CLOB_API_URL points at a reserved host: {host}"
    );

    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let o = ip.octets();
        let blocked = o[0] == 0
            || o[0] == 10
            || o[0] == 127
            || (o[0] == 100 && (64..=127).contains(&o[1]))
            || (o[0] == 169 && o[1] == 254)
            || (o[0] == 172 && (16..=31).contains(&o[1]))
            || (o[0] == 192 && o[1] == 168)
            || o[0] >= 224;
        anyhow::ensure!(
            !blocked,
            "CLOB_API_URL points at a loopback/private/reserved address: {ip}"
        );
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        let s = ip.segments();
        let blocked = s == [0, 0, 0, 0, 0, 0, 0, 1]
            || s == [0, 0, 0, 0, 0, 0, 0, 0]
            || (s[0] & 0xfe00) == 0xfc00
            || (s[0] & 0xffc0) == 0xfe80
            || s[..6] == [0, 0, 0, 0, 0, 0xffff];
        anyhow::ensure!(
            !blocked,
            "CLOB_API_URL points at a loopback/link-local address: {ip}"
        );
    }

    if raw.ends_with('/') {
        Ok(raw.to_owned())
    } else {
        Ok(format!("{raw}/"))
    }
}

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
    /// Exercise the venue paths trading actually needs (authenticated balance +
    /// L2-signed sweep GET) and report capability.
    SelfCheck {
        reply: oneshot::Sender<CoreResult<SelfCheckReport>>,
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

    pub async fn self_check(&self) -> CoreResult<SelfCheckReport> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(VenueCmd::SelfCheck { reply: tx })
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
    let url = validate_venue_host(&url)?;

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

    // Sweep transport: our own client with a browser-shaped UA. The SDK's
    // dedicated client is kept for trading; only the read-only /data/* sweep
    // goes through this one so the two paths stay independent.
    let sweep_http = reqwest::Client::builder()
        .user_agent(SWEEP_UA)
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(ACCEPT, "application/json".parse().unwrap());
            h.insert(ACCEPT_LANGUAGE, "en-US,en;q=0.9".parse().unwrap());
            h
        })
        .build()
        .map_err(|e| anyhow::anyhow!("sweep http client: {e}"))?;
    let signer_checksum = signer_address.to_checksum(None);

    tokio::spawn(async move {
        actor_loop(
            client,
            signer,
            funder,
            ws_url,
            markets,
            ws_credentials,
            signer_checksum,
            url,
            sweep_http,
            cmd_rx,
            events,
        )
        .await;
    });

    Ok(handle)
}

#[allow(clippy::too_many_arguments)]
async fn actor_loop<S: alloy::signers::Signer + Clone + Send + Sync + 'static>(
    client: Client<Authenticated<Normal>>,
    signer: S,
    funder: Address,
    ws_url: String,
    markets: Vec<String>,
    ws_credentials: Credentials,
    signer_checksum: String,
    host_url: String,
    sweep_http: reqwest::Client,
    mut cmd_rx: mpsc::Receiver<VenueCmd>,
    events: mpsc::Sender<VenueEvent>,
) {
    // User-WS stream (best effort: skip when no markets are provided).
    if !markets.is_empty()
        && let Err(e) = start_user_ws(funder, ws_url, ws_credentials.clone(), events.clone()).await
    {
        let _ = events
            .send(VenueEvent::Fatal(format!("user ws setup failed: {e}")))
            .await;
    }

    // Sweep log discipline: a 5s loop must not flood the run log — print the
    // sweep summary only when the counts actually change (failures always
    // print via the periodic loop in live.rs).
    let mut last_sweep: Option<(usize, usize, usize)> = None;
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
                let res =
                    sdk_snapshot(&sweep_http, &host_url, &ws_credentials, &signer_checksum).await;
                if let Ok((open, trades)) = &res {
                    let maker = trades.iter().filter(|t| t.maker == Some(true)).count();
                    let key = (open.len(), trades.len(), maker);
                    if last_sweep != Some(key) {
                        eprintln!(
                            "polymarket-extension: sweep: open={} trades={} maker_entries={}",
                            key.0, key.1, key.2
                        );
                        last_sweep = Some(key);
                    }
                }
                let _ = reply.send(res);
            }
            VenueCmd::SelfCheck { reply } => {
                let res = self_check_probe(
                    &client,
                    &sweep_http,
                    &host_url,
                    &ws_credentials,
                    &signer_checksum,
                )
                .await;
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

/// Trading-capability self-check: exercise the venue paths trading actually
/// needs, in order, and report each as a probe item. Deliberately READ-ONLY —
/// no order is ever placed, cancelled or modified by this probe:
///   1. `balance_allowance` (authenticated SDK POST) — proves the L2
///      credentials, the authenticated client and venue reachability, i.e.
///      everything a live POST shares before it can even be signed.
///   2. L2-signed `GET /data/orders` (the sweep transport) — proves the
///      reconciliation channel works; auth failure here means the sweep
///      safety net is blind.
///
/// A probe failure is `ok=false` and the host freezes trading on it.
async fn self_check_probe(
    client: &Client<Authenticated<Normal>>,
    http: &reqwest::Client,
    host_url: &str,
    creds: &Credentials,
    signer_checksum: &str,
) -> CoreResult<SelfCheckReport> {
    let ts = now_epoch_ms();
    let mut items: Vec<SelfCheckItem> = Vec::new();

    match sdk_balance(client).await {
        Ok(b) => items.push(SelfCheckItem {
            name: "balance".into(),
            ok: true,
            detail: format!("venue free balance {b}"),
        }),
        Err(e) => {
            let raw = e.raw.clone().unwrap_or_default();
            items.push(SelfCheckItem {
                name: "balance".into(),
                ok: false,
                detail: format!("{:?}: {} {raw}", e.code, e.message),
            });
        }
    }

    match l2_get_json(http, host_url, creds, signer_checksum, "/data/orders").await {
        Ok(v) => {
            let n = v
                .get("data")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            items.push(SelfCheckItem {
                name: "sweep".into(),
                ok: true,
                detail: format!("/data/orders ok, {n} open order(s)"),
            });
        }
        Err(e) => {
            let raw = e.raw.clone().unwrap_or_default();
            items.push(SelfCheckItem {
                name: "sweep".into(),
                ok: false,
                detail: format!("{:?}: {} {raw}", e.code, e.message),
            });
        }
    }

    let ok = items.iter().all(|i| i.ok);
    Ok(SelfCheckReport {
        ok,
        ts_ms: ts,
        items,
    })
}

pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn sdk_balance(client: &Client<Authenticated<Normal>>) -> CoreResult<Decimal> {
    let req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    let bal = client
        .balance_allowance(req)
        .await
        .map_err(|e| map_sdk_err(&e.to_string()))?;
    // The CLOB reports collateral in USDC base units (6 decimals); the ledger
    // runs in plain dollars, so scale exactly once at this boundary.
    let usdc_micros = Decimal::new(1_000_000, 0);
    let raw = bal.balance;
    if raw != raw.trunc() {
        // A fractional balance would mean the API stopped reporting base
        // units — refuse to guess the scale rather than misstate the ledger.
        return Err(CoreError::new(
            CoreErrorCode::VenueError,
            format!("balance not in USDC base units: {raw}"),
        ));
    }
    Ok(raw / usdc_micros)
}

/// Periodic REST sweep: open orders + executed trades, fetched as raw JSON via
/// our own L2-signed client.
///
/// Why not the SDK's `orders()/trades()`: their response schema insists every
/// decimal be a parseable fixed-point string, and the venue legitimately
/// returns `"size": ""` on some trade rows (proven 2026-09-20: every sweep
/// failed with `invalid value: string ""`, so the fill safety net never ran).
/// Lenient parsing skips the junk rows instead of losing the whole sweep.
async fn sdk_snapshot(
    http: &reqwest::Client,
    host_url: &str,
    creds: &Credentials,
    signer_checksum: &str,
) -> CoreResult<(Vec<String>, Vec<VenueTradeInfo>)> {
    let open = l2_get_json(http, host_url, creds, signer_checksum, "/data/orders").await?;
    let open_ids: Vec<String> = as_slice(open.get("data"))
        .iter()
        .filter_map(|o| {
            let id = str_field(o, "id")?;
            let live = str_field(o, "status")
                .unwrap_or("LIVE")
                .eq_ignore_ascii_case("LIVE");
            live.then(|| id.to_string())
        })
        .collect();

    let page = l2_get_json(http, host_url, creds, signer_checksum, "/data/trades").await?;
    let mut trades = Vec::new();
    for t in as_slice(page.get("data")) {
        if str_field(t, "status").is_some_and(|s| s.eq_ignore_ascii_case("FAILED")) {
            continue;
        }
        // One direction for the whole trade; our maker legs are its inverse.
        // Derived rather than trusted from the REST per-maker side field —
        // the WS path (`trade_fills`) has no per-maker side at all and uses
        // the same derivation, so both channels agree on semantics.
        let Some(side) = side_field(t) else { continue };
        let ts_ms = match t.get("match_time") {
            Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(serde_json::Value::Number(n)) => n.as_i64(),
            _ => None,
        };
        let Some(ts_ms) = ts_ms else { continue };
        let ts_ms = ts_ms.saturating_mul(1000);
        let trade_id = str_field(t, "id").unwrap_or_default().to_string();

        if let (Some(taker_id), Some(size), Some(price)) = (
            str_field(t, "taker_order_id"),
            dec_field(t, "size"),
            dec_field(t, "price"),
        ) {
            trades.push(VenueTradeInfo {
                venue_order_id: taker_id.to_string(),
                trade_id: trade_id.clone(),
                token_id: str_field(t, "asset_id").unwrap_or_default().to_string(),
                side,
                size,
                price,
                ts_ms,
                tx_hash: None,
                maker: Some(false),
            });
        }
        for m in as_slice(t.get("maker_orders")) {
            let Some(order_id) = str_field(m, "order_id") else {
                continue;
            };
            let Some(matched) = dec_field(m, "matched_amount") else {
                continue;
            };
            if matched <= Decimal::ZERO {
                continue;
            }
            let Some(price) = dec_field(m, "price") else {
                continue;
            };
            trades.push(VenueTradeInfo {
                venue_order_id: order_id.to_string(),
                trade_id: format!("{trade_id}:{order_id}"),
                token_id: str_field(m, "asset_id").unwrap_or_default().to_string(),
                side: side.invert(),
                size: matched,
                price,
                ts_ms,
                tx_hash: None,
                maker: Some(true),
            });
        }
    }
    Ok((open_ids, trades))
}

/// One L2-signed GET against the CLOB, returning the raw JSON body. The header
/// scheme mirrors the SDK's `auth::l2::create_headers`: HMAC-SHA256 over
/// `"{timestamp}GET{path}"` (GET has no body; the path excludes the query),
/// secret and signature both base64url. POLY_ADDRESS is the signer's
/// checksummed address (the SDK stores `state.address = signer.address()`);
/// POLY_TIMESTAMP is the raw `/time` value in milliseconds, matching the SDK's
/// `use_server_time` behaviour.
async fn l2_get_json(
    http: &reqwest::Client,
    host_url: &str,
    creds: &Credentials,
    signer_checksum: &str,
    path: &str,
) -> CoreResult<serde_json::Value> {
    let ts = http
        .get(format!("{host_url}time"))
        .send()
        .await
        .map_err(|e| CoreError::new(CoreErrorCode::VenueError, format!("/time fetch: {e}")))?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| CoreError::new(CoreErrorCode::VenueError, format!("/time body: {e}")))?
        .as_i64()
        .ok_or_else(|| CoreError::new(CoreErrorCode::VenueError, "/time: not a number"))?;

    let decoded_secret = URL_SAFE
        .decode(creds.secret().expose_secret())
        .map_err(|e| CoreError::new(CoreErrorCode::NotAuthenticated, format!("api secret: {e}")))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&decoded_secret)
        .map_err(|e| CoreError::new(CoreErrorCode::NotAuthenticated, format!("hmac key: {e}")))?;
    mac.update(format!("{ts}GET{path}").as_bytes());
    let sig = URL_SAFE.encode(mac.finalize().into_bytes());

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "POLY_ADDRESS",
        signer_checksum.parse().expect("POLY_ADDRESS"),
    );
    headers.insert(
        "POLY_API_KEY",
        creds.key().to_string().parse().expect("POLY_API_KEY"),
    );
    headers.insert(
        "POLY_PASSPHRASE",
        creds
            .passphrase()
            .expose_secret()
            .parse()
            .expect("POLY_PASSPHRASE"),
    );
    headers.insert("POLY_SIGNATURE", sig.parse().expect("POLY_SIGNATURE"));
    headers.insert(
        "POLY_TIMESTAMP",
        ts.to_string().parse().expect("POLY_TIMESTAMP"),
    );

    let resp = http
        .get(format!("{host_url}{path}"))
        .headers(headers)
        .send()
        .await
        .map_err(|e| CoreError::new(CoreErrorCode::VenueError, format!("sweep {path}: {e}")))?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| {
        CoreError::new(CoreErrorCode::VenueError, format!("sweep {path} body: {e}"))
    })?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        let head = &text[..text.len().min(300)];
        return Err(CoreError::new(
            CoreErrorCode::VenueError,
            format!("sweep {path} -> status {status}: {head}"),
        ));
    }
    serde_json::from_slice(&body).map_err(|e| {
        CoreError::new(
            CoreErrorCode::VenueError,
            format!("sweep {path} decode: {e}"),
        )
    })
}

fn as_slice(v: Option<&serde_json::Value>) -> &[serde_json::Value] {
    v.and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn side_field(v: &serde_json::Value) -> Option<Side> {
    match v.get("side")?.as_str()? {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

/// Lenient decimal: numbers via their shortest string form (no float noise),
/// empty strings — which the venue sends on some rows — yield None and the
/// row gets skipped instead of killing the sweep.
fn dec_field(v: &serde_json::Value, key: &str) -> Option<rust_decimal::Decimal> {
    match v.get(key) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            rust_decimal::Decimal::from_str(s.trim()).ok()
        }
        Some(serde_json::Value::Number(n)) => rust_decimal::Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

/// Subscribe the authenticated user channel and translate SDK messages into
/// venue events. The stream ends only on error; a real deployment should
/// reconnect with backoff (the actor keeps accepting commands meanwhile).
async fn start_user_ws(
    funder: Address,
    ws_url: String,
    ws_credentials: Credentials,
    events: mpsc::Sender<VenueEvent>,
) -> anyhow::Result<()> {
    let ws = WsClient::new(&ws_url, WsConfig::default())?.authenticate(ws_credentials, funder)?;
    // Subscribe to the account-wide user stream (empty market filter = all
    // user events). Rounds roll every few minutes and each is a NEW market
    // with fresh condition ids, so a subscription fixed at startup would
    // never see fills for later rounds. Order/trade matching, dedup and
    // unknown-order buffering live on the OME side.
    let stream = ws.subscribe_user_events(Vec::new())?;
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
