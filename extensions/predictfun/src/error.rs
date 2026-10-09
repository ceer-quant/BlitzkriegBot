//! predict.fun error taxonomy → kernel [`CoreError`] mapping (#423).
//!
//! The kernel never sees a raw HTTP status or a raw venue JSON body: every
//! failure crossing this boundary is a [`CoreError`] with a kernel
//! [`CoreErrorCode`], a human message, and the raw venue text preserved in
//! `CoreError::raw` (the kernel's contract: raw text is kept, never altered
//! or hidden). A transport failure that bypasses this mapping is a red-bar
//! test.
//!
//! predict.fun answers failures with one structured envelope shape
//! (`{"success":false,"code":<int>,"error":"<slug>","message":"<text>"}` —
//! measured against the production edge, since the venue publishes no API
//! reference), so [`PredictError::Api`] carries the slug and the numeric
//! code together and the mapper reads both.

use blitzkrieg_market_api::{CoreError, CoreErrorCode};

/// Everything that can go wrong inside the predict.fun transport.
#[derive(Debug, thiserror::Error)]
pub enum PredictError {
    /// HTTP status the venue answered with (any status; mapped below).
    #[error("predict http {status}: {body}")]
    Http { status: u16, body: String },
    /// The venue answered with its structured error envelope
    /// (`error` slug + numeric `code` from the JSON body).
    #[error("predict api {error_slug}: {message}")]
    Api {
        error_slug: String,
        message: String,
        code: u16,
    },
    /// Request exceeded its deadline. Never retried past the budget.
    #[error("predict request timed out after {ms}ms")]
    Timeout { ms: u64 },
    /// Rate limiter exhausted its burst and the backoff budget.
    #[error("predict rate limited after {attempts} attempts")]
    RateLimited { attempts: u32 },
    /// Transport-level failure (connect, TLS, reset, decode).
    #[error("predict transport: {0}")]
    Transport(String),
    /// Credentials absent or malformed. Configuration error, not retried.
    #[error("predict auth config: {0}")]
    AuthConfig(String),
    /// Response body did not match the expected schema.
    #[error("predict schema: {0}")]
    Schema(String),
}

/// The venue's error envelope as it appears inside an HTTP failure body.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct VenueEnvelope {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub code: Option<u16>,
}

impl VenueEnvelope {
    /// Lift a JSON failure body into the typed [`PredictError::Api`] variant.
    /// Any body that does not parse as the envelope degrades to a plain
    /// [`PredictError::Http`] with the raw text kept — never a fake success,
    /// never a panic.
    pub fn into_error(status: u16, body: &str) -> PredictError {
        match serde_json::from_str::<VenueEnvelope>(body) {
            Ok(env) => PredictError::Api {
                error_slug: env.error.unwrap_or_else(|| format!("http_{status}")),
                message: env
                    .message
                    .unwrap_or_else(|| String::from("(venue sent no message)")),
                code: env.code.unwrap_or(status),
            },
            Err(_) => PredictError::Http {
                status,
                body: body.to_string(),
            },
        }
    }
}

impl PredictError {
    /// True for failures a bounded retry may legitimately repair: transport
    /// resets, gateway-class 5xx and 429 (after the limiter's own backoff).
    /// 4xx other than 429 are the caller's fault — retrying them only hammers
    /// the venue, so they are terminal.
    pub fn retryable(&self) -> bool {
        match self {
            PredictError::Http { status, .. } => *status == 429 || *status >= 500,
            PredictError::Api { code, .. } => *code == 429 || *code >= 500,
            PredictError::Timeout { .. } => false, // the deadline already elapsed
            PredictError::RateLimited { .. } => false,
            PredictError::Transport(_) => true,
            PredictError::AuthConfig(_) | PredictError::Schema(_) => false,
        }
    }

    /// True when the limiter (not the venue) produced the refusal.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, PredictError::RateLimited { .. })
            || matches!(self, PredictError::Http { status: 429, .. })
            || matches!(self, PredictError::Api { code: 429, .. })
    }
}

/// Map any predict.fun failure onto the kernel's unified error type. This is
/// the ONLY door an error may use to reach the kernel.
pub fn into_core_error(e: &PredictError) -> CoreError {
    let (code, message): (CoreErrorCode, String) = match e {
        PredictError::Timeout { ms } => (
            CoreErrorCode::Timeout,
            format!("predict.fun 请求超时（{ms}ms）"),
        ),
        PredictError::RateLimited { attempts } => (
            CoreErrorCode::VenueError,
            format!("predict.fun 限频：退避 {attempts} 次后仍被拒"),
        ),
        PredictError::AuthConfig(msg) => (
            CoreErrorCode::NotAuthenticated,
            format!("predict.fun 认证配置：{msg}"),
        ),
        PredictError::Api {
            error_slug,
            message,
            ..
        } => (map_api_slug(error_slug), message.clone()),
        PredictError::Http { status, .. } => {
            (map_status(*status), format!("predict.fun HTTP {status}"))
        }
        PredictError::Transport(msg) => (
            CoreErrorCode::VenueError,
            format!("predict.fun 连接失败：{msg}"),
        ),
        PredictError::Schema(msg) => (
            CoreErrorCode::Internal,
            format!("predict.fun 响应无法解析：{msg}"),
        ),
    };
    CoreError::new(code, message).with_raw(e.to_string())
}

/// predict.fun's structured error slugs (subset that matters operationally)
/// onto kernel codes. Unknown slugs fall back to `VenueError` — never panic,
/// never leak a raw string as the kernel-facing message.
fn map_api_slug(slug: &str) -> CoreErrorCode {
    match slug {
        "unauthorized" | "forbidden" | "invalid_api_key" | "jwt_expired" => {
            CoreErrorCode::NotAuthenticated
        }
        "insufficient_balance" | "insufficient_funds" => CoreErrorCode::InsufficientFunds,
        "not_found" | "market_not_found" | "market_closed" => CoreErrorCode::MarketHalted,
        "bad_request" | "invalid_order" | "invalid_price" | "invalid_amount" => {
            CoreErrorCode::InvalidParams
        }
        "rate_limited" | "too_many_requests" => CoreErrorCode::VenueError,
        _ => CoreErrorCode::VenueError,
    }
}

fn map_status(status: u16) -> CoreErrorCode {
    match status {
        401 | 403 => CoreErrorCode::NotAuthenticated,
        404 => CoreErrorCode::UnknownOrder,
        429 => CoreErrorCode::VenueError,
        s if s >= 500 => CoreErrorCode::VenueError,
        _ => CoreErrorCode::InvalidParams,
    }
}
