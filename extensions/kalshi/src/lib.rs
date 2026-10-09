//! Kalshi market extension for the Blitzkrieg trading core.
//!
//! Stage 1 (#423) shipped the transport layer: REST, RSA request signing, a
//! read/write token-bucket rate limiter, bounded retries with exponential
//! backoff, hard timeouts, and the mapping of every Kalshi error onto the
//! kernel's [`CoreError`].
//!
//! Stage 2 (#424) adds the market-plugin components behind the
//! `blitzkrieg-market-api` contract: the orderbook feed ([`feed`]), event
//! discovery ([`discovery`]) and the network probe ([`net_check`]), bundled by
//! [`plugin::KalshiPlugin`]. Everything the core sees is the market seam; all
//! Kalshi knowledge lives here. Normalization happens at the plugin boundary
//! (cents → 0..1 probabilities), so the kernel never sees a Kalshi-native
//! unit. The order executor arrives with stage 4 (#426).
//!
//! Credentials come from the process environment only (`KALSHI_API_KEY_ID`,
//! `KALSHI_RSA_PRIVATE_KEY` in PEM). No credential material may appear in
//! source, examples or tests — the mock-server test suite runs entirely
//! without it (unauthenticated paths and forged-header assertions).

pub mod discovery;
pub mod error;
pub mod feed;
pub mod net_check;
pub mod plugin;
pub mod rate_limit;
pub mod rest;
pub mod sign;
pub mod types;

pub use error::{KalshiError, into_core_error};
pub use feed::{feed_tests_only_cents_to_prob, feed_tests_only_spawn};
pub use plugin::KalshiPlugin;
pub use rate_limit::{RateLimit, RateLimitConfig};
pub use rest::{KalshiRest, RestConfig};
pub use types::{KalshiBalance, KalshiMarket, KalshiOrder, KalshiOrderBook};
