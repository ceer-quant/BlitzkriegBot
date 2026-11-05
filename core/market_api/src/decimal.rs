//! Decimal wire helpers — compatibility facade (issue #840 Wave 1).
//!
//! The canonical serde helpers moved verbatim to
//! `blitzkrieg-bk-contract::decimal`; this module re-exports them so every
//! `blitzkrieg_market_api::decimal::…` path (including the `#[serde(with =
//! "…")]` references inside the contract's own DTOs and the core's mirrors)
//! keeps resolving.

pub use blitzkrieg_bk_contract::decimal::*;
