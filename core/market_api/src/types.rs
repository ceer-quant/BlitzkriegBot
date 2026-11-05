//! Market plugin domain types — compatibility facade (issue #840 Wave 1).
//!
//! The canonical definitions moved verbatim to `blitzkrieg-bk-contract::types`;
//! this module re-exports the full surface so
//! `blitzkrieg_market_api::types::…` keeps resolving with one definition
//! behind it.

pub use blitzkrieg_bk_contract::types::*;
