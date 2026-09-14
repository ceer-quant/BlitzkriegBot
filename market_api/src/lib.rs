//! `blitzkrieg-market-api` — the market-plugin contract.
//!
//! Market-agnostic domain DTOs plus the software seam that lets a specific venue
//! (Polymarket, Binance, …) live in its own crate while the trading core stays
//! venue-free. This crate deliberately has **no internal dependencies**, so both
//! `blitzkrieg-core` and every `extensions/*` crate can depend on it without
//! forming a Cargo dependency cycle.

pub mod decimal;
pub mod plugin;
pub mod types;

pub use plugin::{BoxFuture, DataFeed, MarketDiscovery, MarketHost, MarketPlugin, OrderExecutor, SubscriptionControl};
pub use types::*;
