//! `blitzkrieg-market-api` — the market-plugin contract.
//!
//! Market-agnostic domain DTOs plus the software seam that lets a specific venue
//! (Polymarket, Binance, …) live in its own crate while the trading core stays
//! venue-free. This crate deliberately has **no internal dependencies**, so both
//! `blitzkrieg-core` and every `extensions/*` crate can depend on it without
//! forming a Cargo dependency cycle.

pub mod account;
pub mod decimal;
pub mod kline;
pub mod modes;
pub mod net;
pub mod plugin;
pub mod types;

pub use account::{AccountId, DEFAULT_ACCOUNT_ID, default_account_id};
pub use kline::{Kline, KlineInterval};
pub use modes::{MarketCapabilities, MarketMode, MarketStructure, ModeError};
pub use plugin::{
    BoxFuture, DataFeed, MarketDiscovery, MarketHost, MarketPlugin, OrderExecutor,
    SubscriptionControl,
};
pub use types::*;
