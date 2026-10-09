//! REST client for the Kalshi exchange API (#423).
//!
//! Transport contract (stage 1 scope):
//! - every request carries a hard timeout (`RestConfig::timeout`);
//! - read/write classes share one RSA-signed header builder but draw from
//!   separate token buckets;
//! - retryable failures (transport, HTTP 429/5xx) are retried with
//!   exponential backoff up to `max_retries`; everything else surfaces
//!   immediately as a typed [`crate::KalshiError`];
//! - order placement sends a client-generated idempotency id, so a timeout
//!   on the submit path can be safely re-queried instead of re-placed.
//!
//! The base URL defaults to the production exchange host and can be pointed
//! at a mock server for tests. Only `https` is accepted outside tests.

use crate::error::KalshiError;
use crate::rate_limit::{Class, RateLimit};
use crate::sign::RequestSigner;
use crate::types::{KalshiBalance, KalshiMarket, KalshiOrder, KalshiOrderBook};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::time::Duration;

/// Production exchange API root.
pub const PRODUCTION_BASE_URL: &str = "https://api.elections.kalshi.com";

#[derive(Debug, Clone)]
pub struct RestConfig {
    pub base_url: String,
    pub timeout: Duration,
    pub max_retries: u32,
    pub backoff_initial: Duration,
    pub backoff_factor: f64,
    /// When true, never route requests through any proxy (env vars or the
    /// OS system configuration). The mock-server tests must set this: an
    /// ambient system proxy would otherwise intercept loopback requests.
    /// Production defaults to false so operators behind a forced proxy work.
    pub no_proxy: bool,
    pub rate_limit: crate::RateLimitConfig,
}

impl Default for RestConfig {
    fn default() -> Self {
        Self {
            base_url: String::from(PRODUCTION_BASE_URL),
            timeout: Duration::from_secs(10),
            max_retries: 3,
            backoff_initial: Duration::from_millis(250),
            backoff_factor: 2.0,
            rate_limit: crate::RateLimitConfig::default(),
            no_proxy: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Envelope<T> {
    // Kalshi wraps list endpoints in `{ "<name>": [...], ... }`; this keeps the
    // wrapper while ignoring extra paging fields.
    #[serde(flatten)]
    inner: T,
}

#[derive(Debug, Clone, Deserialize)]
struct MarketsBody {
    markets: Vec<KalshiMarket>,
}

#[derive(Debug, Clone, Deserialize)]
struct EventsBody {
    events: Vec<EventRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct EventRow {
    event_ticker: String,
    markets: Option<Vec<KalshiMarket>>,
}

#[derive(Debug, Clone, Deserialize)]
struct OrderBody {
    order: KalshiOrder,
}

#[derive(Debug, Clone, Deserialize)]
struct OrdersBody {
    orders: Vec<KalshiOrder>,
}

#[derive(Debug, Clone, Deserialize)]
struct BalanceBody {
    balance: Decimal,
    portfolio_value: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
struct PlaceAck {
    order: KalshiOrder,
    // The venue sometimes echoes the id at the top level instead of inside
    // `order`; parsed so unknown-shape acks fail at the schema check below,
    // not silently.
    #[allow(dead_code)]
    order_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct PlaceOrderPayload<'a> {
    ticker: &'a str,
    client_order_id: &'a str,
    side: &'a str,
    action: &'a str,
    count: u64,
    r#type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    yes_price: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_price: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiration_ts: Option<i64>,
}

/// Kalshi API credentials: key id + RSA private key body, both sourced from
/// the environment by the caller. Nothing here reads the environment itself.
#[derive(Clone)]
pub struct Credentials {
    pub key_id: String,
    pub key_pem: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

pub struct KalshiRest {
    http: reqwest::Client,
    base_url: String,
    signer: Option<RequestSigner>,
    rate_limit: RateLimit,
    timeout: Duration,
    max_retries: u32,
    backoff_initial: Duration,
    backoff_factor: f64,
}

impl std::fmt::Debug for KalshiRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KalshiRest")
            .field("base_url", &self.base_url)
            .field("authenticated", &self.signer.is_some())
            .finish_non_exhaustive()
    }
}

impl KalshiRest {
    /// Build a client. Without [`Credentials`] the client can only hit
    /// public endpoints (markets, order books) — portfolio calls fail with
    /// [`KalshiError::AuthConfig`].
    pub fn new(config: RestConfig, credentials: Option<Credentials>) -> Result<Self, KalshiError> {
        Self::validate_base_url(&config.base_url)?;
        let signer = match credentials {
            Some(creds) => Some(RequestSigner::from_pem(creds.key_id, &creds.key_pem)?),
            None => None,
        };
        let mut builder = reqwest::Client::builder().timeout(config.timeout);
        // reqwest reads env proxies AND (on macOS) the system proxy config;
        // when the operator's ambient proxy is down or refuses loopback,
        // every mock-server request dies through it. The tests opt out
        // unconditionally; production keeps the default.
        if config.no_proxy {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|e| KalshiError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            signer,
            rate_limit: RateLimit::new(config.rate_limit),
            timeout: config.timeout,
            max_retries: config.max_retries,
            backoff_initial: config.backoff_initial,
            backoff_factor: config.backoff_factor,
        })
    }

    fn validate_base_url(url: &str) -> Result<(), KalshiError> {
        let parsed = url
            .parse::<url::Url>()
            .map_err(|_| KalshiError::AuthConfig(String::from("base_url 不是合法 URL")))?;
        if parsed.scheme() == "https" {
            return Ok(());
        }
        // Plain http is only tolerated on loopback — the offline test harness
        // (127.0.0.1 mock server). Any non-loopback host must be https.
        let loopback = parsed
            .host_str()
            .and_then(|h| h.parse::<std::net::IpAddr>().ok())
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
            || parsed.host_str() == Some("localhost");
        if parsed.scheme() == "http" && loopback {
            return Ok(());
        }
        Err(KalshiError::AuthConfig(String::from(
            "base_url 只允许 https",
        )))
    }

    // ── public market data ───────────────────────────────────────────────

    /// Markets for one event ticker.
    pub async fn markets_for_event(
        &self,
        event_ticker: &str,
    ) -> Result<Vec<KalshiMarket>, KalshiError> {
        let path = format!(
            "/trade-api/v2/markets?event_ticker={}",
            urlencode(event_ticker)
        );
        let body: Envelope<MarketsBody> = self.get_json(Class::Read, &path).await?;
        Ok(body.inner.markets)
    }

    /// Every market under every event on the page (discovery uses this).
    pub async fn events_with_markets(
        &self,
        limit: u32,
    ) -> Result<Vec<(String, Vec<KalshiMarket>)>, KalshiError> {
        let path = format!(
            "/trade-api/v2/events?with_nested_markets=true&limit={}",
            limit.clamp(1, 200)
        );
        let body: Envelope<EventsBody> = self.get_json(Class::Read, &path).await?;
        Ok(body
            .inner
            .events
            .into_iter()
            .map(|e| {
                let markets = e.markets.unwrap_or_default();
                (e.event_ticker, markets)
            })
            .collect())
    }

    /// Full-depth order book for one market.
    pub async fn orderbook(&self, ticker: &str) -> Result<KalshiOrderBook, KalshiError> {
        let path = format!("/trade-api/v2/markets/{}/orderbook", urlencode(ticker));
        self.get_json(Class::Read, &path).await
    }

    // ── portfolio (authenticated) ────────────────────────────────────────

    pub async fn balance(&self) -> Result<KalshiBalance, KalshiError> {
        self.require_auth()?;
        let body: Envelope<BalanceBody> = self
            .get_json(Class::Read, "/trade-api/v2/portfolio/get_balance")
            .await?;
        Ok(KalshiBalance {
            balance: body.inner.balance,
            portfolio_value: body.inner.portfolio_value,
        })
    }

    /// Open orders for one (or all) tickers.
    pub async fn orders(&self, ticker: Option<&str>) -> Result<Vec<KalshiOrder>, KalshiError> {
        self.require_auth()?;
        let path = match ticker {
            Some(t) => format!("/trade-api/v2/portfolio/orders?ticker={}", urlencode(t)),
            None => String::from("/trade-api/v2/portfolio/orders"),
        };
        let body: Envelope<OrdersBody> = self.get_json(Class::Read, &path).await?;
        Ok(body.inner.orders)
    }

    /// Place a limit order with a client-generated idempotency id. A timeout
    /// here does NOT imply the order was or was not accepted — the caller is
    /// expected to reconcile by `client_order_id` (see `orders` above).
    #[allow(clippy::too_many_arguments)]
    pub async fn place_limit_order(
        &self,
        ticker: &str,
        side_yes: bool,
        action_buy: bool,
        count: u64,
        price_cents: u64,
        client_order_id: &str,
        expiration_ts: Option<i64>,
    ) -> Result<KalshiOrder, KalshiError> {
        self.require_auth()?;
        let payload = PlaceOrderPayload {
            ticker,
            client_order_id,
            side: if side_yes { "yes" } else { "no" },
            action: if action_buy { "buy" } else { "sell" },
            count,
            r#type: "limit",
            yes_price: if side_yes { Some(price_cents) } else { None },
            no_price: if side_yes { None } else { Some(price_cents) },
            expiration_ts,
        };
        let ack: PlaceAck = self
            .post_json(Class::Write, "/trade-api/v2/portfolio/orders", &payload)
            .await?;
        Ok(ack.order)
    }

    /// Cancel by venue order id; idempotent on the venue (second cancel is a
    /// no-op, not an error we retry on).
    pub async fn cancel_order(&self, order_id: &str) -> Result<KalshiOrder, KalshiError> {
        self.require_auth()?;
        let path = format!("/trade-api/v2/portfolio/orders/{}", urlencode(order_id));
        let body: Envelope<OrderBody> = self
            .request_json(Class::Write, reqwest::Method::DELETE, &path, None::<&()>)
            .await?;
        Ok(body.inner.order)
    }

    // ── plumbing ─────────────────────────────────────────────────────────

    fn require_auth(&self) -> Result<(), KalshiError> {
        if self.signer.is_some() {
            Ok(())
        } else {
            Err(KalshiError::AuthConfig(String::from(
                "该端点需要 KALSHI_API_KEY_ID / KALSHI_RSA_PRIVATE_KEY",
            )))
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        class: Class,
        path: &str,
    ) -> Result<T, KalshiError> {
        self.request_json(class, reqwest::Method::GET, path, None::<&()>)
            .await
    }

    async fn post_json<T: serde::de::DeserializeOwned, P: serde::Serialize>(
        &self,
        class: Class,
        path: &str,
        payload: &P,
    ) -> Result<T, KalshiError> {
        self.request_json(class, reqwest::Method::POST, path, Some(payload))
            .await
    }

    async fn request_json<T, P>(
        &self,
        class: Class,
        method: reqwest::Method,
        path: &str,
        payload: Option<&P>,
    ) -> Result<T, KalshiError>
    where
        T: serde::de::DeserializeOwned,
        P: serde::Serialize,
    {
        let url = format!("{}{}", self.base_url, path);
        let mut attempts = 0u32;
        let mut backoff = self.backoff_initial;
        loop {
            self.rate_limit.acquire(class).await?;
            let sent_at = std::time::Instant::now();
            let outcome = self.one_attempt(&method, &url, payload).await;
            attempts += 1;
            match outcome {
                Ok(text) => {
                    return serde_json::from_str(&text)
                        .map_err(|e| KalshiError::Schema(format!("{}: {}", path, e)));
                }
                Err(err) => {
                    if err.is_rate_limited() {
                        return Err(err);
                    }
                    if !err.retryable() || attempts > self.max_retries {
                        return Err(err);
                    }
                    // A timeout that already consumed most of the budget is
                    // not retried into another full timeout.
                    if matches!(err, KalshiError::Timeout { .. })
                        && sent_at.elapsed() >= self.timeout
                        && attempts >= self.max_retries
                    {
                        return Err(err);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.mul_f64(self.backoff_factor);
                }
            }
        }
    }

    async fn one_attempt<P: serde::Serialize>(
        &self,
        method: &reqwest::Method,
        url: &str,
        payload: Option<&P>,
    ) -> Result<String, KalshiError> {
        let mut request = self.http.request(method.clone(), url);
        if let Some(signer) = &self.signer {
            let timestamp_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default();
            let signature = signer.timestamp_token(timestamp_ms)?;
            request = request
                .header("KALSHI-ACCESS-KEY", signer.key_id())
                .header("KALSHI-ACCESS-SIGNATURE", signature)
                .header("KALSHI-ACCESS-TIMESTAMP", timestamp_ms.to_string());
        }
        let payload_type = if payload.is_some() {
            Some(mime::APPLICATION_JSON)
        } else {
            None
        };
        if let Some(json_mime) = payload_type {
            request = request.header(reqwest::header::CONTENT_TYPE, json_mime.to_string());
        }
        if let Some(body_payload) = payload {
            let text = serde_json::to_string(body_payload)
                .map_err(|e| KalshiError::Schema(format!("payload serialize: {}", e)))?;
            request = request.body(text);
        }
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| KalshiError::Timeout {
                ms: self.timeout.as_millis() as u64,
            })?
            .map_err(|e| classify_send_error(e, self.timeout))?;
        let status = response.status();
        let body_text = response
            .text()
            .await
            .map_err(|e| KalshiError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(KalshiError::Http {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Ok(body_text)
    }
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// reqwest's own request timeout (set on the client builder) surfaces as a
/// plain send error, indistinguishable by string from a reset — and it races
/// our explicit `tokio::time::timeout`. Classify it as a Timeout so retry
/// budgeting and kernel mapping see the deadline, not a generic transport
/// failure.
fn classify_send_error(e: reqwest::Error, configured: Duration) -> KalshiError {
    if e.is_timeout() {
        return KalshiError::Timeout {
            ms: configured.as_millis() as u64,
        };
    }
    KalshiError::Transport(e.to_string())
}
