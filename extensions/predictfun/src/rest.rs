//! REST client for the predict.fun API (#423).
//!
//! Transport contract (stage 1 scope) — identical in shape to the Kalshi
//! client so stage 2 wires both through one aggregation path:
//! - every request carries a hard timeout (`RestConfig::timeout`);
//! - read/write classes draw from separate token buckets;
//! - retryable failures (transport, HTTP 429/5xx) are retried with
//!   exponential backoff up to `max_retries`; everything else surfaces
//!   immediately as a typed [`crate::PredictError`];
//! - order placement carries the caller's EIP-712 order hash; a timeout on
//!   the submit path does NOT imply the order was or was not accepted — the
//!   caller reconciles by hash (`orders` + [`PredictOrder::matches_hash`]).
//!
//! Auth shape: `x-api-key` on every request when a key is configured, plus
//! `Authorization: Bearer <jwt>` once a JWT is set (the caller mints it via
//! `/v1/auth/message` + `/v1/auth` — see [`auth_message`] and
//! [`exchange_jwt`]). Without either, only public endpoints work; portfolio
//! calls fail with [`PredictError::AuthConfig`].
//!
//! The base URL defaults to the production host and can be pointed at a mock
//! server for tests. Only `https` is accepted outside loopback.

use crate::error::{PredictError, VenueEnvelope};
use crate::rate_limit::{Class, RateLimit};
use crate::types::{
    PredictBalance, PredictMarket, PredictOrder, PredictOrderBook, PredictPosition,
};
use serde::Deserialize;
use std::time::Duration;

/// Production REST API root (measured live; the venue publishes no docs page).
pub const PRODUCTION_BASE_URL: &str = "https://api.predict.fun";

/// Production WebSocket root (orderbook topic stream).
pub const PRODUCTION_WS_URL: &str = "wss://ws.predict.fun/ws";

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

// The venue wraps every success payload in `{ "success": true, "data": ... }`
// (measured against the production edge). There is no bare-payload shape: a
// body without `data` is a schema error, never silently parsed as an
// all-defaults payload. A `#[serde(flatten)]`-based optional wrapper would
// also force a `Default` bound on `T` — another reason to keep it required.
#[derive(Debug, Clone, Deserialize)]
struct Envelope<T> {
    #[serde(default = "default_true")]
    #[allow(dead_code)]
    success: bool,
    data: T,
}

fn default_true() -> bool {
    true
}

impl<T> Envelope<T> {
    fn into_inner(self, what: &str) -> T {
        let _ = what; // the schema error already names the endpoint (serde's message)
        self.data
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MarketsBody {
    #[serde(default)]
    markets: Vec<PredictMarket>,
    // The venue has also paged lists under `items`; accept either so a
    // venue-side rename degrades to a schema error, not a silent empty list.
    #[serde(default)]
    items: Option<Vec<PredictMarket>>,
}

impl MarketsBody {
    fn into_rows(self) -> Vec<PredictMarket> {
        match self.items {
            Some(items) => items,
            None => self.markets,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct OrderBody {
    order: PredictOrder,
}

#[derive(Debug, Clone, Deserialize)]
struct OrdersBody {
    #[serde(default)]
    orders: Vec<PredictOrder>,
}

#[derive(Debug, Clone, Deserialize)]
struct PositionsBody {
    #[serde(default)]
    positions: Vec<PredictPosition>,
}

#[derive(Debug, Clone, Deserialize)]
struct BalanceBody {
    balance: rust_decimal::Decimal,
    #[serde(default)]
    total_value: Option<rust_decimal::Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthMessageBody {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthTokenBody {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    jwt: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

impl AuthTokenBody {
    fn into_token(self) -> Option<String> {
        self.token.or(self.jwt).or(self.access_token)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct AuthExchangePayload<'a> {
    signer: &'a str,
    signature: &'a str,
    message: &'a str,
}

/// predict.fun credentials: API key + JWT bearer, both sourced from the
/// environment by the caller. Nothing here reads the environment itself.
/// The JWT is short-lived; [`PredictRest::set_jwt`] rotates it in place.
#[derive(Clone, Default)]
pub struct Credentials {
    pub api_key: Option<String>,
    pub jwt: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("has_api_key", &self.api_key.is_some())
            .field("has_jwt", &self.jwt.is_some())
            .finish_non_exhaustive()
    }
}

pub struct PredictRest {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    jwt: tokio::sync::RwLock<Option<String>>,
    rate_limit: RateLimit,
    timeout: Duration,
    max_retries: u32,
    backoff_initial: Duration,
    backoff_factor: f64,
}

impl std::fmt::Debug for PredictRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PredictRest")
            .field("base_url", &self.base_url)
            .field("has_api_key", &self.api_key.is_some())
            .finish_non_exhaustive()
    }
}

impl PredictRest {
    /// Build a client. Without [`Credentials`] the client can only hit
    /// public endpoints (markets, order books) — portfolio calls fail with
    /// [`PredictError::AuthConfig`].
    pub fn new(config: RestConfig, credentials: Option<Credentials>) -> Result<Self, PredictError> {
        Self::validate_base_url(&config.base_url)?;
        let creds = credentials.unwrap_or_default();
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
            .map_err(|e| PredictError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: creds.api_key,
            jwt: tokio::sync::RwLock::new(creds.jwt),
            rate_limit: RateLimit::new(config.rate_limit),
            timeout: config.timeout,
            max_retries: config.max_retries,
            backoff_initial: config.backoff_initial,
            backoff_factor: config.backoff_factor,
        })
    }

    fn validate_base_url(url: &str) -> Result<(), PredictError> {
        let parsed = url
            .parse::<url::Url>()
            .map_err(|_| PredictError::AuthConfig(String::from("base_url 不是合法 URL")))?;
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
        Err(PredictError::AuthConfig(String::from(
            "base_url 只允许 https",
        )))
    }

    // ── auth bootstrap ───────────────────────────────────────────────────

    /// Fetch the message the venue wants signed (EIP-191 personal_sign) to
    /// mint a JWT. Public endpoint; needs only the API key.
    pub async fn auth_message(&self) -> Result<String, PredictError> {
        let body: Envelope<AuthMessageBody> =
            self.get_json(Class::Read, "/v1/auth/message").await?;
        let inner = body.into_inner("auth/message");
        inner
            .message
            .ok_or_else(|| PredictError::Schema(String::from("auth/message: 无 message 字段")))
    }

    /// Exchange a signature for the JWT bearer. The SIGNING happens in the
    /// caller (stage 2 owns the wallet) — this crate never holds a private
    /// key, only the signature it hands over.
    pub async fn exchange_jwt(
        &self,
        signer_address: &str,
        signature: &str,
        message: &str,
    ) -> Result<String, PredictError> {
        let payload = AuthExchangePayload {
            signer: signer_address,
            signature,
            message,
        };
        let body: Envelope<AuthTokenBody> =
            self.post_json(Class::Write, "/v1/auth", &payload).await?;
        let inner = body.into_inner("auth");
        inner
            .into_token()
            .ok_or_else(|| PredictError::Schema(String::from("auth: 回执无 token 字段")))
    }

    /// Rotate the bearer token in place (JWTs are short-lived).
    pub async fn set_jwt(&self, jwt: String) {
        *self.jwt.write().await = Some(jwt);
    }

    // ── public market data ───────────────────────────────────────────────

    /// All tradeable markets (one page; paging is stage 2 discovery's job).
    pub async fn markets(&self, limit: u32) -> Result<Vec<PredictMarket>, PredictError> {
        let path = format!("/v1/markets?limit={}", limit.clamp(1, 200));
        let body: Envelope<MarketsBody> = self.get_json(Class::Read, &path).await?;
        Ok(body.into_inner("markets").into_rows())
    }

    /// One market (token) by id.
    pub async fn market(&self, token_id: &str) -> Result<PredictMarket, PredictError> {
        let path = format!("/v1/markets/{}", urlencode(token_id));
        let body: Envelope<PredictMarket> = self.get_json(Class::Read, &path).await?;
        Ok(body.into_inner("market"))
    }

    /// Full-depth order book for one token.
    pub async fn orderbook(&self, token_id: &str) -> Result<PredictOrderBook, PredictError> {
        let path = format!("/v1/markets/{}/orderbook", urlencode(token_id));
        let body: Envelope<PredictOrderBook> = self.get_json(Class::Read, &path).await?;
        Ok(body.into_inner("orderbook"))
    }

    // ── portfolio (authenticated) ────────────────────────────────────────

    pub async fn balance(&self) -> Result<PredictBalance, PredictError> {
        self.require_auth().await?;
        let body: Envelope<BalanceBody> = self.get_json(Class::Read, "/v1/balance").await?;
        let inner = body.into_inner("balance");
        Ok(PredictBalance {
            balance: inner.balance,
            total_value: inner.total_value,
        })
    }

    /// Open orders for one (or all) tokens.
    pub async fn orders(&self, token_id: Option<&str>) -> Result<Vec<PredictOrder>, PredictError> {
        self.require_auth().await?;
        let path = match token_id {
            Some(t) => format!("/v1/orders?token_id={}", urlencode(t)),
            None => String::from("/v1/orders"),
        };
        let body: Envelope<OrdersBody> = self.get_json(Class::Read, &path).await?;
        Ok(body.into_inner("orders").orders)
    }

    /// Outcome-token positions.
    pub async fn positions(&self) -> Result<Vec<PredictPosition>, PredictError> {
        self.require_auth().await?;
        let body: Envelope<PositionsBody> = self.get_json(Class::Read, "/v1/positions").await?;
        Ok(body.into_inner("positions").positions)
    }

    /// Submit a signed (EIP-712) order. `order_hash` is the client-side
    /// idempotency key: a timeout here does NOT imply the order was or was
    /// not accepted — reconcile by hash (see `orders`) instead of re-placing.
    /// The order body itself is built by stage 2's executor; this layer only
    /// carries it.
    pub async fn place_order(
        &self,
        order_body: serde_json::Value,
        order_hash: &str,
    ) -> Result<PredictOrder, PredictError> {
        self.require_auth().await?;
        let payload = serde_json::json!({
            "data": {
                "order": order_body,
                "hash": order_hash,
            }
        });
        let body: Envelope<OrderBody> =
            self.post_json(Class::Write, "/v1/orders", &payload).await?;
        Ok(body.into_inner("place order").order)
    }

    /// Cancel by venue order ids; idempotent on the venue.
    pub async fn cancel_orders(&self, ids: &[&str]) -> Result<(), PredictError> {
        self.require_auth().await?;
        let payload = serde_json::json!({ "ids": ids });
        let _: Envelope<serde_json::Value> = self
            .post_json(Class::Write, "/v1/orders/remove", &payload)
            .await?;
        Ok(())
    }

    // ── plumbing ─────────────────────────────────────────────────────────

    async fn require_auth(&self) -> Result<(), PredictError> {
        if self.api_key.is_none() && self.jwt.read().await.is_none() {
            return Err(PredictError::AuthConfig(String::from(
                "该端点需要 PREDICT_API_KEY / PREDICT_JWT_TOKEN",
            )));
        }
        Ok(())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        class: Class,
        path: &str,
    ) -> Result<T, PredictError> {
        self.request_json(class, reqwest::Method::GET, path, None::<&()>)
            .await
    }

    async fn post_json<T: serde::de::DeserializeOwned, P: serde::Serialize>(
        &self,
        class: Class,
        path: &str,
        payload: &P,
    ) -> Result<T, PredictError> {
        self.request_json(class, reqwest::Method::POST, path, Some(payload))
            .await
    }

    async fn request_json<T, P>(
        &self,
        class: Class,
        method: reqwest::Method,
        path: &str,
        payload: Option<&P>,
    ) -> Result<T, PredictError>
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
                        .map_err(|e| PredictError::Schema(format!("{}: {}", path, e)));
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
                    if matches!(err, PredictError::Timeout { .. })
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
    ) -> Result<String, PredictError> {
        let mut request = self.http.request(method.clone(), url);
        if let Some(key) = &self.api_key {
            request = request.header("x-api-key", key);
        }
        if let Some(jwt) = self.jwt.read().await.as_deref() {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"));
        }
        if payload.is_some() {
            request = request.header(
                reqwest::header::CONTENT_TYPE,
                mime::APPLICATION_JSON.to_string(),
            );
        }
        if let Some(body_payload) = payload {
            let text = serde_json::to_string(body_payload)
                .map_err(|e| PredictError::Schema(format!("payload serialize: {}", e)))?;
            request = request.body(text);
        }
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| PredictError::Timeout {
                ms: self.timeout.as_millis() as u64,
            })?
            .map_err(|e| classify_send_error(e, self.timeout))?;
        let status = response.status();
        let body_text = response
            .text()
            .await
            .map_err(|e| PredictError::Transport(e.to_string()))?;
        if !status.is_success() {
            // The venue's structured envelope is the typed error; anything
            // else falls back to a plain Http error with the raw body kept.
            return Err(VenueEnvelope::into_error(status.as_u16(), &body_text));
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
fn classify_send_error(e: reqwest::Error, configured: Duration) -> PredictError {
    if e.is_timeout() {
        return PredictError::Timeout {
            ms: configured.as_millis() as u64,
        };
    }
    PredictError::Transport(e.to_string())
}
