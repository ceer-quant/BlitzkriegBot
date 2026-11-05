//! `blitzkrieg-market-api` — the market-plugin contract.
//!
//! Market-agnostic domain DTOs plus the software seam that lets a specific venue
//! (Polymarket, Binance, …) live in its own crate while the trading core stays
//! venue-free. This crate deliberately has **no internal dependencies**, so both
//! `blitzkrieg-core` and every `extensions/*` crate can depend on it without
//! forming a Cargo dependency cycle.
//!
//! Since issue #840 (Wave 1) the types and traits below live canonically in
//! `blitzkrieg-bk-contract`; this crate re-exports every old path unchanged
//! (`pub use blitzkrieg_bk_contract::…`), so consumers compile untouched with a
//! single definition behind them. What remains implemented HERE is the unified
//! mapping ledger (`std::fs` persistence is implementation, not contract) —
//! re-exported into the `unified` facade so
//! `blitzkrieg_market_api::unified::ledger` keeps resolving.

pub use blitzkrieg_bk_contract::account;
pub use blitzkrieg_bk_contract::decimal;
pub use blitzkrieg_bk_contract::kline;
pub use blitzkrieg_bk_contract::modes;
pub use blitzkrieg_bk_contract::net;
pub use blitzkrieg_bk_contract::plugin;
pub use blitzkrieg_bk_contract::types;

pub mod unified;

pub use blitzkrieg_bk_contract::account::{AccountId, DEFAULT_ACCOUNT_ID, default_account_id};
pub use blitzkrieg_bk_contract::kline::{Kline, KlineInterval};
pub use blitzkrieg_bk_contract::modes::{
    MarketCapabilities, MarketMode, MarketStructure, ModeError,
};
pub use blitzkrieg_bk_contract::plugin::{
    BoxFuture, DataFeed, MarketDiscovery, MarketHost, MarketPlugin, OrderExecutor,
    SubscriptionControl,
};
pub use blitzkrieg_bk_contract::types::*;
pub use unified::{HedgeVerdict, UnifiedEvent};
