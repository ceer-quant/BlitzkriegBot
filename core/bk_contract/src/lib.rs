//! `blitzkrieg-bk-contract` — the canonical market-plugin contract (issue #840,
//! Wave 1).
//!
//! This crate owns the types and traits that cross the market-plugin boundary;
//! `blitzkrieg_market_api` re-exports them unchanged (`pub use
//! blitzkrieg_bk_contract::…`), so every existing import path keeps compiling
//! with one single definition behind it. The crate is deliberately a leaf:
//! deps = serde/serde_json/rust_decimal only, no internal dependencies, and
//! nothing here performs I/O — the unified mapping ledger's std::fs
//! persistence stays in `blitzkrieg-market-api` under
//! `blitzkrieg_market_api::unified::ledger`.
//!
//! The module tree mirrors the old `blitzkrieg_market_api` layout verbatim, so
//! doc links inside the files that moved (`super::matching::…`,
//! `crate::net::…`) resolve exactly as they did before the move.

pub mod account;
pub mod decimal;
pub mod kline;
pub mod modes;
pub mod net;
pub mod plugin;
pub mod types;
pub mod unified;

pub use account::{AccountId, DEFAULT_ACCOUNT_ID, default_account_id};
pub use kline::{Kline, KlineInterval};
pub use modes::{MarketCapabilities, MarketMode, MarketStructure, ModeError};
pub use plugin::{
    BoxFuture, DataFeed, MarketDiscovery, MarketHost, MarketPlugin, OrderExecutor,
    SubscriptionControl,
};
pub use types::*;
pub use unified::{HedgeVerdict, UnifiedEvent};
