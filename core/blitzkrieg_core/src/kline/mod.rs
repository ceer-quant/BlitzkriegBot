//! K-line domain module — DEV_V0_3 §10.2 / §14.0.
//!
//! The `Kline` / `KlineInterval` **types** live in `blitzkrieg_market_api`
//! (`market_api/src/kline.rs`) and are re-exported here, for the same reason
//! `MarketStructure` / `AccountId` are (DEV_V0_3 §11.1 / A.1.6): the strategy
//! API needs these types for `SafeStrategy::on_kline`, and the strategy API may
//! NOT depend on the core. `blitzkrieg_market_api` is the one crate both sides
//! may link, so the shared type lives there and each side re-exports it —
//! `blitzkrieg_core::kline::Kline` and `blitzkrieg_strategy_api::Kline` are the
//! SAME type, not two mirrors that can drift.
//!
//! Wave 0 froze only the types (§14.0 table rows 5). The aggregator
//! (`KlineAggregator`, `on_trade` / `dropped_out_of_order` / `AggregatorStats`),
//! the `engine.rs` feed point and the `kline.*` IPC surface land with **E29** in
//! `kline/mod.rs`'s sibling file `kline/aggregator.rs`; `bk.kline` for Lua
//! lands with **E30** on top of it. Only "types and signatures" arrive here.

pub use blitzkrieg_market_api::kline::{Kline, KlineInterval};
