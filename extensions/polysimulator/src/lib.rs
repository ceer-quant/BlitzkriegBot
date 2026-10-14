//! PolySimulator market extension for the Blitzkrieg trading core (#454).
//!
//! A complete market-plugin over a SIMULATED venue: PolySimulator mirrors
//! Polymarket's API shape (CLOB books, PM-shape data channels) but fills are
//! the venue engine's own model and balances are play money — the plugin
//! declares the `simulated` capability bit so every surface can label it
//! honestly.
//!
//! The transport needs no signature layer: the venue is single-secret
//! (`X-API-Key`) and its native `POST /v1/order` form accepts orders without
//! EIP-712 material. Components:
//!   * [`feed`] — REST book polling (`GET /v1/clob/book/{token_id}`), prices
//!     pass through the 0..1 band;
//!   * [`discovery`] — round discovery off the venue's own up/down endpoint
//!     (`GET /v1/markets/updown`), whose nested `outcomes`/`outcome_prices`/
//!     `token_ids` are JSON-encoded strings parsed here, at the boundary;
//!   * [`executor`] — REST polling actor: native order placement (`FAK`
//!     taker / `GTC` maker), reconcile over PM-shape trade rows attributed
//!     through BOTH leg sides (`taker_order_id` + `maker_orders`), orphan
//!     cleanup, balance sync;
//!   * [`net_check`] — staged read-only probe.
//!
//! Credentials come from the process environment only (`POLYSIM_API_KEY`).
//! No credential material may appear in source, examples or tests — the
//! mock-server test suite runs entirely without it (unauthenticated paths
//! and forged-header assertions).

pub mod discovery;
pub mod error;
pub mod executor;
pub mod feed;
pub mod net_check;
pub mod plugin;
pub mod rate_limit;
pub mod rest;
pub mod types;

pub use error::{PolySimError, into_core_error};
pub use feed::{feed_tests_only_band_to_prob, feed_tests_only_spawn};
pub use plugin::PolySimulatorPlugin;
pub use rate_limit::{RateLimit, RateLimitConfig};
pub use rest::{Credentials, PolySimRest, RestConfig};
pub use types::{
    BalanceBody, NativeOrder, OrderBookSnapshot, OrderResponse, PmTradeRow, PositionRow, UpDownRow,
};
