//! Market-mode declarations — compatibility facade (issue #840 Wave 1).
//!
//! `MarketStructure`/`MarketCapabilities`/`MarketMode`/`ModeError` and the
//! declaration parser moved verbatim to `blitzkrieg-bk-contract::modes`; this
//! module re-exports the full surface so every
//! `blitzkrieg_market_api::modes::…` path keeps resolving with one definition
//! behind it.

pub use blitzkrieg_bk_contract::modes::*;
