//! REST client for the PolySimulator API (#454).
//!
//! Transport contract (same discipline as the kalshi client):
//! - every request carries a hard timeout (`RestConfig::timeout`);
//! - read/write classes share one `X-API-Key` header but draw from separate
//!   token buckets;
//! - retryable failures (transport, HTTP 429/5xx) are retried with
//!   exponential backoff up to `max_retries`; everything else surfaces
//!   immediately as a typed [`crate::PolySimError`];
//! - order placement sends a `client_order_id`, so a timeout on the submit
//!   path can be safely re-queried instead of re-placed.
//!
//! No signature layer: this venue is single-secret (`X-API-Key`) and its
//! native `POST /v1/order` accepts the order without any EIP-712 material —
//! unlike Polymarket proper. The base URL defaults to the production host and
//! can be pointed at a mock server for tests. Only `https` is accepted
//! outside loopback tests.

use crate::error::PolySimError;
use crate::rate_limit::{Class, RateLimit};
use crate::types::{
    BalanceBody, BookLevel, CancelOneBody, NativeOrder, OrderBookSnapshot, OrderResponse,
    PendingOrdersEnvelope, PlaceOrderBody, PmTradesEnvelope, PositionRow, UpDownEnvelope,
};
use rust_decimal::Decimal;
use std::time::Duration;

/// Production API root.
pub const PRODUCTION_BASE_URL: &str = "https://api.polysimulator.com";

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

/// PolySimulator API key — sourced from the environment by the caller.
/// Nothing here reads the environment itself.
#[derive(Clone)]
pub struct Credentials {
    pub api_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

pub struct PolySimRest {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    rate_limit: RateLimit,
    timeout: Duration,
    max_retries: u32,
    backoff_initial: Duration,
    backoff_factor: f64,
}

impl std::fmt::Debug for PolySimRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolySimRest")
            .field("base_url", &self.base_url)
            .field("authenticated", &self.api_key.is_some())
            .finish_non_exhaustive()
    }
}

impl PolySimRest {
    /// Build a client. Without [`Credentials`] the client can only hit
    /// public endpoints; most account endpoints need the key.
    pub fn new(config: RestConfig, credentials: Option<Credentials>) -> Result<Self, PolySimError> {
        Self::validate_base_url(&config.base_url)?;
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
            .map_err(|e| PolySimError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: credentials.map(|c| c.api_key),
            rate_limit: RateLimit::new(config.rate_limit),
            timeout: config.timeout,
            max_retries: config.max_retries,
            backoff_initial: config.backoff_initial,
            backoff_factor: config.backoff_factor,
        })
    }

    fn validate_base_url(url: &str) -> Result<(), PolySimError> {
        let parsed = url
            .parse::<url::Url>()
            .map_err(|_| PolySimError::AuthConfig(String::from("base_url 不是合法 URL")))?;
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
        Err(PolySimError::AuthConfig(String::from(
            "base_url 只允许 https",
        )))
    }

    // ── public market data ───────────────────────────────────────────────

    /// Active up/down interval markets (optionally filtered).
    pub async fn updown_markets(
        &self,
        asset: Option<&str>,
        interval: Option<&str>,
    ) -> Result<Vec<crate::types::UpDownRow>, PolySimError> {
        let mut path = String::from("/v1/markets/updown?limit=500");
        if let Some(a) = asset {
            path.push_str(&format!("&asset={}", urlencode(a)));
        }
        if let Some(i) = interval {
            path.push_str(&format!("&interval={}", urlencode(i)));
        }
        let body: UpDownEnvelope = self.get_json(Class::Read, &path).await?;
        Ok(body.markets.unwrap_or_default())
    }

    /// Full-depth L2 book for one CLOB token.
    pub async fn book(&self, token_id: &str) -> Result<OrderBookSnapshot, PolySimError> {
        let path = format!("/v1/clob/book/{}", urlencode(token_id));
        self.get_json(Class::Read, &path).await
    }

    // ── trading (authenticated) ──────────────────────────────────────────

    /// Account endpoints and order writes require the API key. A client built
    /// without one fails HERE — a local AuthConfig, no request leaves — so a
    /// misconfigured deploy hammers nothing and reports the real cause.
    fn require_key(&self) -> Result<(), PolySimError> {
        if self.api_key.is_some() {
            return Ok(());
        }
        Err(PolySimError::AuthConfig(String::from(
            "该端点需要 POLYSIM_API_KEY（客户端未配置凭据）",
        )))
    }

    /// Place an order in the venue's NATIVE form. `time_in_force` decides
    /// the character: `GTC` rests as a maker order, `FAK` crosses as an
    /// immediate taker fill (venue default for market orders).
    pub async fn place_order(
        &self,
        body: &PlaceOrderBody<'_>,
    ) -> Result<OrderResponse, PolySimError> {
        self.require_key()?;
        self.post_json(Class::Write, "/v1/order", body).await
    }

    /// Cancel one order by venue id (the single-order form takes a body with
    /// the venue's odd `orderID` capitalization).
    pub async fn cancel_order(&self, order_id: &str) -> Result<OrderResponse, PolySimError> {
        self.require_key()?;
        let body = CancelOneBody {
            order_id: order_id.to_string(),
        };
        self.request_json(
            Class::Write,
            reqwest::Method::DELETE,
            "/v1/order",
            Some(&body),
        )
        .await
    }

    /// Open (PENDING) orders, native envelope. Empty when nothing rests.
    pub async fn open_orders(&self) -> Result<Vec<NativeOrder>, PolySimError> {
        self.require_key()?;
        let body: PendingOrdersEnvelope = self
            .get_json(Class::Read, "/v1/orders?status=PENDING&limit=100")
            .await?;
        Ok(body.orders.unwrap_or_default())
    }

    /// Recent executed trades, PM-shape envelope (the reconcile unit). The
    /// `next_cursor` paging of the venue is not followed here: the sweep
    /// dedupes by trade id, and one page is far more than a sweep cadence
    /// produces.
    pub async fn trades(&self) -> Result<Vec<crate::types::PmTradeRow>, PolySimError> {
        self.require_key()?;
        let body: PmTradesEnvelope = self
            .get_json(Class::Read, "/v1/data/trades?limit=100")
            .await?;
        Ok(body.data.unwrap_or_default())
    }

    /// Account balance (the ledger seed / free-balance truth).
    pub async fn balance(&self) -> Result<BalanceBody, PolySimError> {
        self.require_key()?;
        self.get_json(Class::Read, "/v1/account/balance").await
    }

    /// Open positions (the reconcile surface for crash recovery).
    pub async fn positions(&self) -> Result<Vec<PositionRow>, PolySimError> {
        self.require_key()?;
        let path = "/v1/account/positions?status=OPEN&limit=100";
        let rows: Vec<PositionRow> = self.get_json(Class::Read, path).await?;
        Ok(rows)
    }

    /// Resolve a CLOB token id to `(market_id, outcome label)` — the two
    /// fields the native order form needs that a token id does not carry.
    /// Public endpoint (no API key), the venue's own answer, so the executor
    /// never guesses an outcome label.
    pub async fn markets_by_token(
        &self,
        token_id: &str,
    ) -> Result<crate::types::MarketByToken, PolySimError> {
        let path = format!("/v1/markets-by-token/{}", urlencode(token_id));
        self.get_json(Class::Read, &path).await
    }

    // ── plumbing ─────────────────────────────────────────────────────────

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        class: Class,
        path: &str,
    ) -> Result<T, PolySimError> {
        self.request_json(class, reqwest::Method::GET, path, None::<&()>)
            .await
    }

    async fn post_json<T: serde::de::DeserializeOwned, P: serde::Serialize>(
        &self,
        class: Class,
        path: &str,
        payload: &P,
    ) -> Result<T, PolySimError> {
        self.request_json(class, reqwest::Method::POST, path, Some(payload))
            .await
    }

    async fn request_json<T, P>(
        &self,
        class: Class,
        method: reqwest::Method,
        path: &str,
        payload: Option<&P>,
    ) -> Result<T, PolySimError>
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
                        .map_err(|e| PolySimError::Schema(format!("{}: {}", path, e)));
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
                    if matches!(err, PolySimError::Timeout { .. })
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
    ) -> Result<String, PolySimError> {
        let mut request = self.http.request(method.clone(), url);
        if let Some(key) = &self.api_key {
            request = request.header("X-API-Key", key);
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
                .map_err(|e| PolySimError::Schema(format!("payload serialize: {}", e)))?;
            request = request.body(text);
        }
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| PolySimError::Timeout {
                ms: self.timeout.as_millis() as u64,
            })?
            .map_err(|e| classify_send_error(e, self.timeout))?;
        let status = response.status();
        let body_text = response
            .text()
            .await
            .map_err(|e| PolySimError::Transport(e.to_string()))?;
        if !status.is_success() {
            // The venue's structured error body is `{error: CODE, message}`;
            // parse it so the kernel sees the venue's own code, falling back
            // to the raw text when the body is not that shape.
            let structured = serde_json::from_str::<crate::types::ApiErrorBody>(&body_text).ok();
            if let Some(err_body) = structured.filter(|b| !b.error.is_empty()) {
                return Err(PolySimError::Api {
                    code: err_body.error,
                    message: err_body.message.unwrap_or(body_text),
                    status: status.as_u16(),
                });
            }
            return Err(PolySimError::Http {
                status: status.as_u16(),
                body: body_text,
            });
        }
        Ok(body_text)
    }
}

/// reqwest's own request timeout (set on the client builder) surfaces as a
/// plain send error, indistinguishable by string from a reset — and it races
/// our explicit `tokio::time::timeout`. Classify it as a Timeout so retry
/// budgeting and kernel mapping see the deadline, not a generic transport
/// failure.
fn classify_send_error(e: reqwest::Error, configured: Duration) -> PolySimError {
    if e.is_timeout() {
        return PolySimError::Timeout {
            ms: configured.as_millis() as u64,
        };
    }
    PolySimError::Transport(e.to_string())
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

/// Re-export for the book feed: the level type the snapshot carries.
pub type BookLevelPair = (Decimal, Decimal);

/// One (price, size) pair from a book side.
pub fn level_pair(l: &BookLevel) -> (Decimal, Decimal) {
    (l.price, l.size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_requires_https_outside_loopback() {
        assert!(PolySimRest::validate_base_url("https://api.polysimulator.com").is_ok());
        assert!(PolySimRest::validate_base_url("http://127.0.0.1:8080").is_ok());
        assert!(PolySimRest::validate_base_url("http://api.example.com").is_err());
        assert!(PolySimRest::validate_base_url("ftp://x").is_err());
    }

    /// The Debug contract: a credential struct renders its secret as a
    /// redaction marker, never the value itself. The key under test is a
    /// synthetic stand-in built at runtime — no credential material in source.
    #[test]
    fn credentials_debug_never_leaks_the_key() {
        let stand_in = format!("{}-{}", "synthetic", "key-under-test");
        let creds = Credentials {
            api_key: stand_in.clone(),
        };
        let rendered = format!("{creds:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains(&stand_in), "{rendered}");
    }
}
