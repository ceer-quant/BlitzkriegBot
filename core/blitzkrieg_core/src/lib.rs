//! blitzkrieg-core: the Rust trading core.
//!
//! All funds / orders / risk / state consistency live in this crate. Node is a
//! thin shell that drives the binary over a Unix-domain-socket JSON-RPC channel.
//!
//! P0 module map:
//!  - `decimal`  money-safe wire (de)serialisation
//!  - `model`    orders, fills, statuses, structured error codes, events
//!  - `ome`      authoritative order state machine + idempotent fill ledger
//!  - `ledger`   USDC reservation / release (prevents overspend)
//!  - `risk`     kill switch + per-order notional gate
//!  - `sim`      dry-mode book-crossing matcher (shares OME with live)
//!  - `service`  typed command layer assembling ome/ledger/risk/sim
//!  - `ipc`      UDS JSON-RPC 2.0 schema + transport

pub mod backtest;
pub mod config;
pub mod data_lock;
pub mod data_source;
pub mod decimal;
pub mod engine;
pub mod exit_policy;
pub mod extension;
pub mod ipc;
pub mod ledger;
pub mod ledger_api;
pub mod logging;
pub mod market;
pub mod marketdata;
pub mod model;
pub mod ome;
pub mod order;
pub mod order_db;
pub mod position;
pub mod position_db;
pub mod reconcile;
pub mod regime_eval;
pub mod risk;
pub mod risk_context;
pub mod scanner;
pub mod service;
pub mod settlement;
pub mod shadow;
pub mod shadow_evolution;
pub mod signal;
pub mod sim;
pub mod strategies;
pub mod strategy_engine;
pub mod strategy_state;
pub mod trade_db;

pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
