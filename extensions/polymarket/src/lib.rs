//! Official Polymarket market extension for the Blitzkrieg trading core.
//!
//! Everything Polymarket-specific lives here: the CLOB venue actor (`venue`),
//! the live bridge (`live`), the orderbook/spot feed (`feed`), Gamma round
//! discovery (`discovery`) and its JSON parsing (`gamma`), all behind the
//! `blitzkrieg-market-api` contract. The core links this crate via its
//! `polymarket` feature and sees only the `MarketPlugin` trait object.

pub mod discovery;
pub mod feed;
pub mod gamma;
pub mod live;
pub mod plugin;
pub mod venue;

pub use plugin::PolymarketPlugin;
