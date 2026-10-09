//! Kalshi error taxonomy → kernel [`CoreError`] mapping (#423).
//!
//! The kernel never sees a raw HTTP status or a raw venue JSON body: every
//! failure crossing this boundary is a [`CoreError`] with a kernel [`CoreErrorCode`],
//! a human message, and the raw venue text preserved in `CoreError::raw`
//! (the kernel's contract: raw text is kept, never altered or hidden).
//! A transport failure that bypasses this mapping is a red-bar test.

use blitzkrieg_market_api::{CoreError, CoreErrorCode};

/// Everything that can go wrong inside the Kalshi transport.
#[derive(Debug, thiserror::Error)]
pub enum KalshiError {
    /// HTTP status the venue answered with (any status; mapped below).
    #[error("kalshi http {status}: {body}")]
    Http { status: u16, body: String },
    /// The venue answered with its structured error envelope.
    #[error("kalshi api {code}: {message}")]
    Api {
        code: String,
        message: String,
        status: u16,
    },
    /// Request exceeded its deadline. Never retried past the budget.
    #[error("kalshi request timed out after {ms}ms")]
    Timeout { ms: u64 },
    /// Rate limiter exhausted its burst and the backoff budget.
    #[error("kalshi rate limited after {attempts} attempts")]
    RateLimited { attempts: u32 },
    /// Transport-level failure (connect, TLS, reset, decode).
    #[error("kalshi transport: {0}")]
    Transport(String),
    /// Credentials absent or malformed. Configuration error, not retried.
    #[error("kalshi auth config: {0}")]
    AuthConfig(String),
    /// Response body did not match the expected schema.
    #[error("kalshi schema: {0}")]
    Schema(String),
}

impl KalshiError {
    /// True for failures a bounded retry may legitimately repair: transport
    /// resets, gateway-class 5xx and 429 (after the limiter's own backoff).
    /// 4xx other than 429 are the caller's fault — retrying them only hammers
    /// the venue, so they are terminal.
    pub fn retryable(&self) -> bool {
        match self {
            KalshiError::Http { status, .. } => *status == 429 || *status >= 500,
            KalshiError::Api { status, .. } => *status == 429 || *status >= 500,
            KalshiError::Timeout { .. } => false, // the deadline already elapsed
            KalshiError::RateLimited { .. } => false,
            KalshiError::Transport(_) => true,
            KalshiError::AuthConfig(_) | KalshiError::Schema(_) => false,
        }
    }

    /// True when the limiter (not the venue) produced the refusal.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, KalshiError::RateLimited { .. })
            || matches!(self, KalshiError::Http { status: 429, .. })
            || matches!(self, KalshiError::Api { status: 429, .. })
    }
}

/// Map any Kalshi failure onto the kernel's unified error type. This is the
/// ONLY door an error may use to reach the kernel.
pub fn into_core_error(e: &KalshiError) -> CoreError {
    let (code, message): (CoreErrorCode, String) = match e {
        KalshiError::Timeout { ms } => {
            (CoreErrorCode::Timeout, format!("Kalshi 请求超时（{ms}ms）"))
        }
        KalshiError::RateLimited { attempts } => (
            CoreErrorCode::VenueError,
            format!("Kalshi 限频：退避 {attempts} 次后仍被拒"),
        ),
        KalshiError::AuthConfig(msg) => (
            CoreErrorCode::NotAuthenticated,
            format!("Kalshi 认证配置：{msg}"),
        ),
        KalshiError::Api { code, message, .. } => (map_api_code(code), message.clone()),
        KalshiError::Http { status, .. } => (map_status(*status), format!("Kalshi HTTP {status}")),
        KalshiError::Transport(msg) => {
            (CoreErrorCode::VenueError, format!("Kalshi 连接失败：{msg}"))
        }
        KalshiError::Schema(msg) => (
            CoreErrorCode::Internal,
            format!("Kalshi 响应无法解析：{msg}"),
        ),
    };
    CoreError::new(code, message).with_raw(e.to_string())
}

/// Kalshi's structured error codes (subset that matters operationally) onto
/// kernel codes. Unknown codes fall back to `VenueError` — never panic, never
/// leak a raw string as the kernel-facing message.
fn map_api_code(code: &str) -> CoreErrorCode {
    match code {
        "AUTHENTICATION_INVALID" | "AUTHORIZATION_INVALID" | "NO_CREDENTIALS" => {
            CoreErrorCode::NotAuthenticated
        }
        "INSUFFICIENT_FUNDS" => CoreErrorCode::InsufficientFunds,
        "ORDER_NOT_FOUND" | "ORDER_ALREADY_CANCELED" => CoreErrorCode::UnknownOrder,
        "MARKET_CLOSED" | "MARKET_NOT_FOUND" | "SERIES_NOT_FOUND" | "EVENT_NOT_FOUND" => {
            CoreErrorCode::MarketHalted
        }
        "BAD_REQUEST" | "INVALID_ORDER" | "INVALID_PRICE" | "INVALID_AMOUNT" => {
            CoreErrorCode::InvalidParams
        }
        "TOO_MANY_REQUESTS" => CoreErrorCode::VenueError,
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
