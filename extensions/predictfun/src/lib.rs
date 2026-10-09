//! predict.fun client for Blitzkrieg — stage 1 of 0.3.3 (#423).
//!
//! Transport layer: REST + WebSocket, `x-api-key` header + JWT bearer
//! auth, a read/write token-bucket rate limiter, bounded retries with
//! exponential backoff, hard timeouts, and the mapping of every predict.fun
//! error onto the kernel's [`CoreError`].
//!
//! Stage 2 (#424) adds the market-plugin components behind the
//! `blitzkrieg-market-api` contract: the orderbook feed ([`feed`]), market
//! discovery ([`discovery`]) and the network probe ([`net_check`]), bundled by
//! [`plugin::PredictFunPlugin`]. Everything the core sees is the market seam;
//! all predict.fun knowledge lives here. Prices are 0..1 probabilities at the
//! venue already — normalization is only the degenerate-band filter — so the
//! kernel never sees a predict.fun-native unit. The order executor arrives
//! with stage 4 (#426).
//!
//! Auth shape (measured against the production edge and pinned here because
//! the venue publishes no API reference): the REST root is
//! `https://api.predict.fun`, unauthenticated paths answer with the venue's
//! structured error envelope (`{"success":false,"code":401,...}`), and
//! authenticated access is `x-api-key: <key>` plus `Authorization: Bearer
//! <jwt>`. The JWT is minted by signing an auth message fetched from
//! `/v1/auth/message` with the account's EIP-191 key and POSTing the
//! signature to `/v1/auth` — this crate performs that exchange and then
//! attaches the bearer token; the signing KEY ITSELF stays out of this crate
//! (the caller hands over an already-usable JWT, sourced from the process
//! environment or a key service). Order placement is an EIP-712 signed order
//! body built by stage 2's executor; stage 1 only carries it and enforces
//! idempotent retry discipline on the submit path.
//!
//! Credentials come from the process environment only (`PREDICT_API_KEY`,
//! `PREDICT_JWT_TOKEN`). No credential material may appear in source,
//! examples or tests — the mock-server test suite runs entirely without it.

pub mod discovery;
pub mod error;
pub mod feed;
pub mod net_check;
pub mod plugin;
pub mod rate_limit;
pub mod rest;
pub mod types;

pub use error::{PredictError, into_core_error};
pub use feed::{feed_tests_only_prob_in_band, feed_tests_only_spawn};
pub use plugin::PredictFunPlugin;
pub use rate_limit::{RateLimit, RateLimitConfig};
pub use rest::{PredictRest, RestConfig};
pub use types::{PredictBalance, PredictMarket, PredictOrder, PredictOrderBook, PredictPosition};
