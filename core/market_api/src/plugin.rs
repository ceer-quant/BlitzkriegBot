//! The plugin contracts — compatibility facade (issue #840 Wave 1).
//!
//! `MarketHost` and the component traits moved verbatim to
//! `blitzkrieg-bk-contract::plugin`; this module re-exports the full surface
//! so `blitzkrieg_market_api::plugin::…` keeps resolving with one definition
//! behind it.

pub use blitzkrieg_bk_contract::plugin::*;
