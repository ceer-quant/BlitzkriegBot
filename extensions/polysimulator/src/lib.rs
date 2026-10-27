//! PolySimulator market extension for the Blitzkrieg trading core (#454) —
//! parasitic form.
//!
//! An execution-only plugin over a SIMULATED venue: PolySimulator mirrors
//! Polymarket's API shape and its token ids byte for byte, fills are the
//! venue engine's own model and balances are play money. The plugin declares
//! the `simulated` capability bit so every surface can label it honestly,
//! and `feed_source = Some("polymarket")` (RFC-661 M2) tells the API layer
//! to reuse polymarket's upstream feed/discovery instead of starting a
//! second one — each round batch lands under THIS venue's own
//! `(venue, token)` rows (the engine holds no mirror facts: RFC-661 M3
//! retired the engine-facing mirror declaration with the whole bridge).
//!
//! The transport needs no signature layer: the venue is single-secret
//! (`X-API-Key`) and its native `POST /v1/order` form accepts orders without
//! EIP-712 material. Components:
//!   * [`executor`] — REST polling actor: native order placement (`FAK`
//!     taker / `GTC` maker), reconcile over PM-shape trade rows attributed
//!     through BOTH leg sides (`taker_order_id` + `maker_orders`), orphan
//!     cleanup, balance sync, and the settlement sweep over
//!     `GET /v1/markets/updown`;
//!   * [`net_check`] — staged read-only probe.
//!
//! Credentials come from the process environment only (`POLYSIM_API_KEY`).
//! No credential material may appear in source, examples or tests — the
//! mock-server test suite runs entirely without it (unauthenticated paths
//! and forged-header assertions).

pub mod error;
pub mod executor;
pub mod net_check;
pub mod plugin;
pub mod rate_limit;
pub mod rest;
pub mod types;

pub use error::{PolySimError, into_core_error};
pub use plugin::PolySimulatorPlugin;
pub use rate_limit::{RateLimit, RateLimitConfig};
pub use rest::{Credentials, PolySimRest, RestConfig};
pub use types::{BalanceBody, NativeOrder, OrderResponse, PmTradeRow, PositionRow, UpDownRow};
