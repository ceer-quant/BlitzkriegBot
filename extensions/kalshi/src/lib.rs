//! Kalshi client for Blitzkrieg — stage 1 of 0.3.3 (#423).
//!
//! This crate is the transport layer only: REST + WebSocket, RSA-PSS request
//! signing, a read/write token-bucket rate limiter, bounded retries with
//! exponential backoff, hard timeouts, and the mapping of every Kalshi error
//! onto the kernel's [`CoreError`]. Strategy wiring, discovery/feed/executor
//! trait impls and the kernel contract live in stage 2 (#424).
//!
//! Credentials come from the process environment only (`KALSHI_API_KEY_ID`,
//! `KALSHI_RSA_PRIVATE_KEY` in PEM). No credential material may appear in
//! source, examples or tests — the mock-server test suite runs entirely
//! without it (unauthenticated paths and forged-header assertions).

pub mod error;
pub mod rate_limit;
pub mod rest;
pub mod sign;
pub mod types;

pub use error::{KalshiError, into_core_error};
pub use rate_limit::{RateLimit, RateLimitConfig};
pub use rest::{KalshiRest, RestConfig};
pub use types::{KalshiBalance, KalshiMarket, KalshiOrder, KalshiOrderBook};
