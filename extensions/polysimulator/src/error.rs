//! PolySimulator error taxonomy → kernel [`CoreError`] mapping (#454).
//!
//! The kernel never sees a raw HTTP status or a raw venue JSON body: every
//! failure crossing this boundary is a [`CoreError`] with a kernel
//! [`CoreErrorCode`], a human message, and the raw venue text preserved in
//! `CoreError::raw`. A transport failure that bypasses this mapping is a
//! red-bar test.

use blitzkrieg_market_api::{CoreError, CoreErrorCode};

/// Everything that can go wrong inside the PolySimulator transport.
#[derive(Debug, thiserror::Error)]
pub enum PolySimError {
    /// HTTP status the venue answered with (any status; mapped below).
    #[error("polysimulator http {status}: {body}")]
    Http { status: u16, body: String },
    /// The venue answered with a structured error body (`{error, message}`).
    #[error("polysimulator api {code}: {message}")]
    Api {
        code: String,
        message: String,
        status: u16,
    },
    /// Request exceeded its deadline. Never retried past the budget.
    #[error("polysimulator request timed out after {ms}ms")]
    Timeout { ms: u64 },
    /// Rate limiter exhausted its burst and the backoff budget.
    #[error("polysimulator rate limited after {attempts} attempts")]
    RateLimited { attempts: u32 },
    /// Transport-level failure (connect, TLS, reset, decode).
    #[error("polysimulator transport: {0}")]
    Transport(String),
    /// API key absent or malformed. Configuration error, not retried.
    #[error("polysimulator auth config: {0}")]
    AuthConfig(String),
    /// Response body did not match the expected schema.
    #[error("polysimulator schema: {0}")]
    Schema(String),
}

impl PolySimError {
    /// True for failures a bounded retry may legitimately repair: transport
    /// resets, gateway-class 5xx and 429 (after the limiter's own backoff).
    /// 4xx other than 429 are the caller's fault — retrying them only hammers
    /// the venue, so they are terminal.
    pub fn retryable(&self) -> bool {
        match self {
            PolySimError::Http { status, .. } => *status == 429 || *status >= 500,
            PolySimError::Api { status, .. } => *status == 429 || *status >= 500,
            PolySimError::Timeout { .. } => false, // the deadline already elapsed
            PolySimError::RateLimited { .. } => false,
            PolySimError::Transport(_) => true,
            PolySimError::AuthConfig(_) | PolySimError::Schema(_) => false,
        }
    }

    /// True when the limiter (not the venue) produced the refusal.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, PolySimError::RateLimited { .. })
            || matches!(self, PolySimError::Http { status: 429, .. })
            || matches!(self, PolySimError::Api { status: 429, .. })
    }
}

/// Map any PolySimulator failure onto the kernel's unified error type. This is
/// the ONLY door an error may use to reach the kernel.
pub fn into_core_error(e: &PolySimError) -> CoreError {
    let (code, message): (CoreErrorCode, String) = match e {
        PolySimError::Timeout { ms } => (
            CoreErrorCode::Timeout,
            format!("PolySimulator 请求超时（{ms}ms）"),
        ),
        PolySimError::RateLimited { attempts } => (
            CoreErrorCode::VenueError,
            format!("PolySimulator 限频：退避 {attempts} 次后仍被拒"),
        ),
        PolySimError::AuthConfig(msg) => (
            CoreErrorCode::NotAuthenticated,
            format!("PolySimulator 认证配置：{msg}"),
        ),
        PolySimError::Api { code, message, .. } => (map_api_code(code), message.clone()),
        PolySimError::Http { status, .. } => {
            (map_status(*status), format!("PolySimulator HTTP {status}"))
        }
        PolySimError::Transport(msg) => (
            CoreErrorCode::VenueError,
            format!("PolySimulator 连接失败：{msg}"),
        ),
        PolySimError::Schema(msg) => (
            CoreErrorCode::Internal,
            format!("PolySimulator 响应无法解析：{msg}"),
        ),
    };
    CoreError::new(code, message).with_raw(e.to_string())
}

/// PolySimulator's structured error codes (subset that matters operationally)
/// onto kernel codes. Unknown codes fall back to `VenueError` — never panic,
/// never leak a raw string as the kernel-facing message.
fn map_api_code(code: &str) -> CoreErrorCode {
    match code {
        "WALLET_NOT_FOUND" | "AUTHENTICATION_REQUIRED" | "UNAUTHORIZED" => {
            CoreErrorCode::NotAuthenticated
        }
        "INSUFFICIENT_BALANCE" | "INSUFFICIENT_FUNDS" => CoreErrorCode::InsufficientFunds,
        "ORDER_NOT_FOUND" | "ORDER_ALREADY_CANCELED" | "ALREADY_CANCELED" => {
            CoreErrorCode::UnknownOrder
        }
        "MARKET_CLOSED" | "MARKET_NOT_FOUND" | "MARKET_RESOLVED" | "NOT_FOUND" => {
            CoreErrorCode::MarketHalted
        }
        "VALIDATION_FAILED"
        | "INVALID_ORDER_MIN_TICK_SIZE"
        | "INVALID_ORDER_EXPIRATION"
        | "INVALID_PRICE"
        | "INVALID_QUANTITY" => CoreErrorCode::InvalidParams,
        // Concurrency and quota are venue-side conditions, not caller bugs.
        "TRADE_IN_PROGRESS" | "FREE_TIER_DAILY_LIMIT" | "RATE_LIMITED" | "TOO_MANY_REQUESTS" => {
            CoreErrorCode::VenueError
        }
        "DUPLICATE_CLIENT_ORDER_ID" => CoreErrorCode::InvalidParams,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_map_onto_kernel_semantics() {
        assert_eq!(
            map_api_code("FREE_TIER_DAILY_LIMIT"),
            CoreErrorCode::VenueError
        );
        assert_eq!(
            map_api_code("INVALID_ORDER_MIN_TICK_SIZE"),
            CoreErrorCode::InvalidParams
        );
        assert_eq!(
            map_api_code("WALLET_NOT_FOUND"),
            CoreErrorCode::NotAuthenticated
        );
        assert_eq!(
            map_api_code("INSUFFICIENT_BALANCE"),
            CoreErrorCode::InsufficientFunds
        );
        assert_eq!(map_api_code("ORDER_NOT_FOUND"), CoreErrorCode::UnknownOrder);
        assert_eq!(map_api_code("weird_new_code"), CoreErrorCode::VenueError);
    }

    #[test]
    fn retry_ladder_matches_the_kalshi_contract() {
        assert!(PolySimError::Transport("reset".into()).retryable());
        assert!(
            PolySimError::Http {
                status: 502,
                body: String::new()
            }
            .retryable()
        );
        assert!(
            !PolySimError::Http {
                status: 400,
                body: String::new()
            }
            .retryable()
        );
        assert!(!PolySimError::Timeout { ms: 1 }.retryable());
        assert!(
            PolySimError::Http {
                status: 429,
                body: String::new()
            }
            .is_rate_limited()
        );
    }
}
