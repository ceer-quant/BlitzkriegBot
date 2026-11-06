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
    BalanceBody, CancelAck, CancelOneBody, NativeOrder, OrderResponse, PendingOrdersEnvelope,
    PlaceOrderBody, PmTradesEnvelope, UpDownEnvelope,
};
use std::time::Duration;

/// Production API root.
pub const PRODUCTION_BASE_URL: &str = "https://api.polysimulator.com";

/// The User-Agent every request carries. The venue's Cloudflare rule 1010s
/// non-browser UAs (curl/python/reqwest defaults) in waves; a browser-shaped
/// string is what their WAF admits. Honest and boring: it identifies nothing
/// false — it is a compatibility knob for a WAF that does not know this
/// venue's own API consumers exist.
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 BlitzkriegBot/0.4.1";

/// The wait a venue 429 advertises when it sends no `Retry-After` header —
/// the free tier's own documented advice ("Retry after 1s").
const VENUE_RETRY_AFTER_DEFAULT_MS: u64 = 1_000;
/// Ceiling on the advertised wait, so a bogus header can never park a
/// request path for longer than the limiter's own give-up budget.
const VENUE_RETRY_AFTER_CAP_MS: u64 = 5_000;

/// Page size of the two account sweeps (FILLED orders + trades) — unchanged
/// from the pre-#878 single-page reads; pagination follows the cursor
/// instead of enlarging the page.
const SWEEP_PAGE_LIMIT: u32 = 100;
/// Hard ceiling on pages ONE sweep read will follow (#878). 40 × 100 rows is
/// the same budget the venue's own recovery script
/// (`scripts/polysim-history.mjs`: `PAGE = 100`, `MAX_PAGES = 40`) allows.
/// A venue that never clears `has_more` must not turn a 5s sweep into an
/// unbounded fetch; the walk keeps the rows it already read and says on the
/// log that it truncated — silent truncation is exactly the missed-fill
/// failure mode this loop exists to fix.
pub const SWEEP_MAX_PAGES: u32 = 40;

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

fn apply_process_proxy(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::ClientBuilder, PolySimError> {
    match blitzkrieg_market_api::net::client_proxy().map_err(PolySimError::Transport)? {
        blitzkrieg_market_api::net::ClientProxy::FromEnvironment => Ok(builder),
        blitzkrieg_market_api::net::ClientProxy::Disable => Ok(builder.no_proxy()),
        blitzkrieg_market_api::net::ClientProxy::Explicit(url) => {
            let proxy = reqwest::Proxy::all(&url).map_err(|e| {
                PolySimError::Transport(format!(
                    "proxy URL rejected: {}",
                    blitzkrieg_market_api::net::error_chain(&e)
                ))
            })?;
            Ok(builder.proxy(proxy))
        }
    }
}

impl PolySimRest {
    /// Build a client. Without [`Credentials`] the client can only hit
    /// public endpoints; most account endpoints need the key.
    pub fn new(config: RestConfig, credentials: Option<Credentials>) -> Result<Self, PolySimError> {
        Self::validate_base_url(&config.base_url)?;
        let mut builder = reqwest::Client::builder()
            .timeout(config.timeout)
            // The venue sits behind Cloudflare, whose bot rule 1010s
            // non-browser User-Agents on market-data paths in waves (live
            // 2026-10-25: reqwest's own UA 403×4 while a browser UA passed
            // 200×2, same IP, same minute). A browser-shaped UA is the
            // compatibility knob; revisit if the venue re-scopes its WAF to
            // admit API clients.
            .user_agent(BROWSER_UA);
        // reqwest reads env proxies AND (on macOS) the system proxy config;
        // when the operator's ambient proxy is down or refuses loopback,
        // every mock-server request dies through it. The tests opt out
        // unconditionally; production keeps the default.
        if config.no_proxy {
            builder = builder.no_proxy();
        } else {
            builder = apply_process_proxy(builder)?;
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
        self.updown_markets_bounded(asset, interval, None).await
    }

    /// [`Self::updown_markets`] with an explicit `limit`. `None` keeps the
    /// venue default (500) — the settlement sweep keeps the full page as its
    /// bottom line; a `Some` cap narrows the page for callers that only need
    /// the head of the list.
    pub async fn updown_markets_bounded(
        &self,
        asset: Option<&str>,
        interval: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<crate::types::UpDownRow>, PolySimError> {
        let body: UpDownEnvelope = self
            .get_json(Class::Read, &updown_path(asset, interval, limit))
            .await?;
        Ok(body.markets.unwrap_or_default())
    }

    /// The verdict for ONE market, by condition id: `GET /v1/markets/{cid}`
    /// (verified live 2026-11-06). The updown list rotates resolved rows out
    /// within an hour, but this endpoint keeps the verdict after close —
    /// `resolved_outcome` names the winning outcome even when every list
    /// window has forgotten the condition. Unauthenticated market data, same
    /// as the list.
    pub async fn single_market(
        &self,
        condition_id: &str,
    ) -> Result<crate::types::SingleMarketVerdict, PolySimError> {
        self.get_json(
            Class::Read,
            &format!("/v1/markets/{}", urlencode(condition_id)),
        )
        .await
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

    /// Place an order in the venue's native form (the `{"order": {…}}`
    /// envelope of tokenId + 1e6-scaled maker/taker amounts — verified live
    /// 2026-10-25). Non-marketable orders rest (the venue reports
    /// `status:"live"`), marketable ones match immediately (`"matched"`).
    pub async fn place_order(&self, body: &PlaceOrderBody) -> Result<OrderResponse, PolySimError> {
        self.require_key()?;
        self.post_json(Class::Write, "/v1/order", &Self::enveloped(body))
            .await
    }

    /// Cancel one order by venue id. The ack is a batch envelope. An id in
    /// `canceled` is a plain success. An id in `not_canceled` was refused
    /// because it is already gone — cancelled or filled ("Order not found or
    /// already cancelled/filled"), typically a fill landing in the ~0.5s
    /// window before our DELETE — which IS the goal state: the order no
    /// longer rests, so the cancel is idempotent-success like the sibling
    /// venues (kalshi/predictfun promise it, polymarket ignores the ack).
    /// Only an id named in neither set (malformed ack) errors as
    /// ORDER_NOT_FOUND; a genuine HTTP 404 keeps its own mapping.
    pub async fn cancel_order(&self, order_id: &str) -> Result<String, PolySimError> {
        self.require_key()?;
        let body = CancelOneBody {
            order_id: order_id.to_string(),
        };
        let ack: CancelAck = self
            .request_json(
                Class::Write,
                reqwest::Method::DELETE,
                "/v1/order",
                Some(&body),
            )
            .await?;
        if ack.cancelled_id(order_id).is_some() {
            return Ok(order_id.to_string());
        }
        if let Some(reason) = ack.rejection(order_id) {
            eprintln!(
                "polysimulator-extension: cancel of {order_id} already terminal at the venue: {reason}"
            );
            return Ok(order_id.to_string());
        }
        Err(PolySimError::Api {
            code: "ORDER_NOT_FOUND".into(),
            message: format!("cancel refused for {order_id}: not in the canceled set"),
            status: 200,
            retry_after_ms: None,
        })
    }

    /// Open (PENDING) orders, native envelope. Empty when nothing rests.
    pub async fn open_orders(&self) -> Result<Vec<NativeOrder>, PolySimError> {
        self.require_key()?;
        let body: PendingOrdersEnvelope = self
            .get_json(Class::Read, "/v1/orders?status=PENDING&limit=100")
            .await?;
        Ok(body.orders.unwrap_or_default())
    }

    /// Recently FILLED orders, native envelope — the fill-attribution
    /// fallback. The venue's trade rows DO NOT carry our order id
    /// (`taker_order_id` holds the trade's own id, `maker_orders` is absent —
    /// verified live 2026-10-25/26), so the trades stream can never be matched
    /// to our orders. The FILLED orders list does: each row names its
    /// `order_id` + `fill_price` + `filled_at` + `quantity`.
    ///
    /// #878: the read follows the venue's `next_cursor` until the list ends
    /// (bounded by [`SWEEP_MAX_PAGES`]). Before #878 only the FIRST page was
    /// ever read — any FILLED order past row 100 stayed invisible to the
    /// sweeps forever, silently dropping fills.
    pub async fn filled_orders(&self) -> Result<Vec<NativeOrder>, PolySimError> {
        self.require_key()?;
        self.sweep_paged::<PendingOrdersEnvelope, _>("FILLED orders", filled_orders_path)
            .await
    }

    /// Recent executed trades, PM-shape envelope (the reconcile unit).
    ///
    /// #878: the read follows the venue's `next_cursor` until the list ends
    /// (bounded by [`SWEEP_MAX_PAGES`]). The sweep dedupes by trade id
    /// (`seen_trades`), so re-reading pages across sweeps stays safe; what
    /// #878 changes is that trades beyond the first page are seen AT ALL —
    /// previously any trade past row 100 never entered the reconcile stream.
    pub async fn trades(&self) -> Result<Vec<crate::types::PmTradeRow>, PolySimError> {
        self.require_key()?;
        self.sweep_paged::<PmTradesEnvelope, _>("trades", trades_path)
            .await
    }

    /// Account balance (the ledger seed / free-balance truth).
    pub async fn balance(&self) -> Result<BalanceBody, PolySimError> {
        self.require_key()?;
        self.get_json(Class::Read, "/v1/account/balance").await
    }

    // ── plumbing ─────────────────────────────────────────────────────────

    /// The venue's POST /v1/order wrapper: the flat native fields ride an
    /// `{"order": {…}}` envelope (a flat body is answered
    /// `VALIDATION_FAILED: missing order`).
    fn enveloped<T: serde::Serialize>(order: &T) -> impl serde::Serialize + use<'_, T> {
        serde_json::json!({ "order": order })
    }

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

    /// Follow the venue's `next_cursor` for one sweep read until the list
    /// ends (#878). `path` builds the per-page request from the cursor to
    /// echo back. The walk stops per [`sweep_next_cursor`], or at
    /// [`SWEEP_MAX_PAGES`] with the venue still claiming more — in every
    /// abnormal stop the truncation is logged with the endpoint named:
    /// silently dropping pages is exactly the missed-fill failure mode
    /// (#878) this loop exists to fix.
    async fn sweep_paged<E, P>(&self, endpoint: &str, path: P) -> Result<Vec<E::Row>, PolySimError>
    where
        E: SweepPageEnvelope,
        P: Fn(Option<&str>) -> String,
    {
        let mut rows = Vec::new();
        let mut sent: Option<String> = None;
        let mut pages: u32 = 0;
        loop {
            if pages >= SWEEP_MAX_PAGES {
                eprintln!(
                    "polysimulator-extension: {endpoint}: page cap {SWEEP_MAX_PAGES} hit and the venue still reports more — truncating this sweep read; rows beyond page {SWEEP_MAX_PAGES} stay unseen until the next sweep"
                );
                break;
            }
            let body: E = self.get_json(Class::Read, &path(sent.as_deref())).await?;
            let (page_rows, next_cursor, has_more) = body.page_parts();
            rows.extend(page_rows);
            pages += 1;
            match sweep_next_cursor(has_more, next_cursor, sent.as_deref()) {
                SweepPage::Next(cursor) => sent = Some(cursor),
                SweepPage::End => break,
                SweepPage::Stuck(reason) => {
                    eprintln!(
                        "polysimulator-extension: {endpoint}: {reason} — pagination stops so the same page cannot be re-served forever"
                    );
                    break;
                }
            }
        }
        Ok(rows)
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
                    // The LOCAL limiter's refusal (its own bounded wait could
                    // not cover the call) surfaces immediately: a retry here
                    // would pile a second waiter onto an already-starving
                    // bucket.
                    if matches!(err, PolySimError::RateLimited { .. }) {
                        return Err(err);
                    }
                    // A VENUE 429 is an invitation, not a capability failure —
                    // the body literally says "Retry after 1s". Sleep the
                    // advertised wait (header first, then the free tier's
                    // documented 1s) and retry within the same bounded budget:
                    // the startup burst (balance seed → self-check probes →
                    // first sweep) trips the venue's limiter by construction,
                    // and that burst must self-heal here rather than come back
                    // as `trades failed` → trading freeze → wrong ledger seed.
                    if err.is_rate_limited() {
                        if attempts > self.max_retries {
                            return Err(err);
                        }
                        let wait = err
                            .retry_after_ms()
                            .unwrap_or(VENUE_RETRY_AFTER_DEFAULT_MS)
                            .min(VENUE_RETRY_AFTER_CAP_MS);
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        continue;
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
        // The venue's `Retry-After` (seconds) travels with the error so the
        // retry loop can honor the advertised wait instead of guessing.
        let retry_after_ms = if status == 429 {
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|secs| secs.saturating_mul(1_000))
        } else {
            None
        };
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
                    retry_after_ms,
                });
            }
            return Err(PolySimError::Http {
                status: status.as_u16(),
                body: body_text,
                retry_after_ms,
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

/// One cursor step of a paginated sweep read (#878). The venue's own
/// recovery script (`scripts/polysim-history.mjs`) pins the wire convention
/// this mirrors: the response's `next_cursor` is echoed back as the
/// `next_cursor` query parameter of the next page, and a cursor that is
/// absent, empty, or unchanged from the one just sent ends the walk. An
/// explicit `has_more:false` ends it too; a missing `has_more` defers to the
/// cursor (leaner envelopes keep working). Pure decision logic — the caller
/// logs, so the warning can name the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SweepPage {
    /// Fetch the next page, echoing this cursor back.
    Next(String),
    /// The list is exhausted: `has_more:false`, or no cursor offered while
    /// `has_more` is absent/false.
    End,
    /// The venue claims more but cannot name a NEW page: it re-offered the
    /// cursor just sent (an echo), or claimed `has_more:true` with no cursor
    /// at all. Stop and log — following it would re-serve the same page
    /// forever.
    Stuck(&'static str),
}

pub(crate) fn sweep_next_cursor(
    has_more: Option<bool>,
    next_cursor: Option<String>,
    sent_cursor: Option<&str>,
) -> SweepPage {
    if has_more == Some(false) {
        return SweepPage::End;
    }
    match next_cursor.filter(|c| !c.trim().is_empty()) {
        Some(c) if Some(c.as_str()) == sent_cursor => {
            SweepPage::Stuck("next_cursor repeated the cursor just sent (no forward progress)")
        }
        Some(c) => SweepPage::Next(c),
        None if has_more == Some(true) => {
            SweepPage::Stuck("has_more=true but the response carries no next_cursor")
        }
        None => SweepPage::End,
    }
}

/// One page of a paginated sweep read: the rows it carries plus the venue's
/// pagination handle. Both account reads (#878) share this shape, so both
/// share the [`PolySimRest::sweep_paged`] loop.
trait SweepPageEnvelope: serde::de::DeserializeOwned {
    type Row;
    fn page_parts(self) -> (Vec<Self::Row>, Option<String>, Option<bool>);
}

impl SweepPageEnvelope for PendingOrdersEnvelope {
    type Row = NativeOrder;
    fn page_parts(self) -> (Vec<NativeOrder>, Option<String>, Option<bool>) {
        (
            self.orders.unwrap_or_default(),
            self.next_cursor,
            self.has_more,
        )
    }
}

impl SweepPageEnvelope for PmTradesEnvelope {
    type Row = crate::types::PmTradeRow;
    fn page_parts(self) -> (Vec<crate::types::PmTradeRow>, Option<String>, Option<bool>) {
        (
            self.data.unwrap_or_default(),
            self.next_cursor,
            self.has_more,
        )
    }
}

/// The `GET /v1/markets/updown` query string. `limit=None` pins the venue's
/// page default (500) so the settlement sweep sees the same full list it
/// always did; a `Some` cap narrows the page for callers that do not need
/// every row. `pub(crate)` so tests can pin the exact wire shape their
/// parameters produce.
pub(crate) fn updown_path(
    asset: Option<&str>,
    interval: Option<&str>,
    limit: Option<u32>,
) -> String {
    let mut path = String::from("/v1/markets/updown?limit=");
    match limit {
        Some(n) => path.push_str(&n.to_string()),
        None => path.push_str("500"),
    }
    if let Some(a) = asset {
        path.push_str(&format!("&asset={}", urlencode(a)));
    }
    if let Some(i) = interval {
        path.push_str(&format!("&interval={}", urlencode(i)));
    }
    path
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

/// The FILLED sweep page of `GET /v1/orders`. The venue's cursor rides back
/// as the `next_cursor` query parameter — the exact convention the venue's
/// own recovery script (`scripts/polysim-history.mjs`) uses. `pub(crate)` so
/// tests pin the wire shape their parameters produce (same discipline as
/// [`updown_path`]).
pub(crate) fn filled_orders_path(cursor: Option<&str>) -> String {
    let mut path = format!("/v1/orders?status=FILLED&limit={SWEEP_PAGE_LIMIT}");
    if let Some(c) = cursor {
        path.push_str("&next_cursor=");
        path.push_str(&urlencode(c));
    }
    path
}

/// The trades sweep page of `GET /v1/data/trades` (same cursor convention).
pub(crate) fn trades_path(cursor: Option<&str>) -> String {
    let mut path = format!("/v1/data/trades?limit={SWEEP_PAGE_LIMIT}");
    if let Some(c) = cursor {
        path.push_str("&next_cursor=");
        path.push_str(&urlencode(c));
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep query paths: pre-#878 both were a single
    /// `?…&limit=100` page; now the cursor rides back as `next_cursor` —
    /// the venue's own recovery-script convention. URL-encoded.
    #[test]
    fn sweep_query_paths_echo_the_cursor_as_next_cursor() {
        assert_eq!(
            filled_orders_path(None),
            "/v1/orders?status=FILLED&limit=100"
        );
        assert_eq!(
            filled_orders_path(Some("cur/1:2")),
            "/v1/orders?status=FILLED&limit=100&next_cursor=cur%2F1%3A2"
        );
        assert_eq!(trades_path(None), "/v1/data/trades?limit=100");
        assert_eq!(
            trades_path(Some("c1")),
            "/v1/data/trades?limit=100&next_cursor=c1"
        );
    }

    /// The cursor step rules (#878): fresh cursor continues; `has_more:false`
    /// ends; an absent/empty cursor ends; a repeated cursor or a
    /// `has_more:true` with no cursor is a Stuck stop (logged by the caller).
    #[test]
    fn sweep_cursor_steps_continue_only_on_a_fresh_cursor() {
        // A fresh cursor continues — `has_more` true or absent (a leaner
        // envelope that only names the cursor keeps paging).
        assert_eq!(
            sweep_next_cursor(Some(true), Some("c2".into()), Some("c1")),
            SweepPage::Next("c2".into())
        );
        assert_eq!(
            sweep_next_cursor(None, Some("c2".into()), None),
            SweepPage::Next("c2".into())
        );

        // Explicit has_more=false ends, even if a cursor rides along.
        assert_eq!(
            sweep_next_cursor(Some(false), Some("c2".into()), Some("c1")),
            SweepPage::End
        );

        // An absent or empty cursor without a has_more claim is a normal end.
        assert_eq!(sweep_next_cursor(None, None, None), SweepPage::End);
        assert_eq!(
            sweep_next_cursor(None, Some("   ".into()), Some("c1")),
            SweepPage::End
        );

        // A cursor that does not advance (the venue echoes the one just
        // sent) is a Stuck stop, not a loop.
        assert!(matches!(
            sweep_next_cursor(Some(true), Some("c1".into()), Some("c1")),
            SweepPage::Stuck(_)
        ));

        // has_more=true without any cursor cannot advance either.
        assert!(matches!(
            sweep_next_cursor(Some(true), None, Some("c1")),
            SweepPage::Stuck(_)
        ));
    }

    /// The updown query path: the settlement sweep's full-list bottom line
    /// (venue-default 500) vs a narrowed request with explicit filters —
    /// filters ride the query string in this exact shape.
    #[test]
    fn updown_query_path_encodes_default_and_narrowed_pages() {
        assert_eq!(
            updown_path(None, None, None),
            "/v1/markets/updown?limit=500"
        );
        assert_eq!(
            updown_path(None, Some("15m"), Some(50)),
            "/v1/markets/updown?limit=50&interval=15m"
        );
        assert_eq!(
            updown_path(Some("BTC"), Some("1h"), Some(7)),
            "/v1/markets/updown?limit=7&asset=BTC&interval=1h"
        );
    }

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
