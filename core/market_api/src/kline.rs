//! K-line domain type — compatibility facade (issue #840 Wave 1).
//!
//! `Kline`/`KlineInterval` moved verbatim to `blitzkrieg-bk-contract::kline`;
//! this module re-exports them so `blitzkrieg_market_api::kline::…` keeps
//! resolving to one single type.

pub use blitzkrieg_bk_contract::kline::*;
