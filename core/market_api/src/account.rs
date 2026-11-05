//! Account identity — compatibility facade (issue #840 Wave 1).
//!
//! `AccountId` and its default moved verbatim to
//! `blitzkrieg-bk-contract::account`; this module re-exports them so
//! `crate::account::AccountId` and `blitzkrieg_market_api::account::AccountId`
//! keep resolving to one single type.

pub use blitzkrieg_bk_contract::account::*;
