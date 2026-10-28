//! Verb-family handlers, split out of `server.rs` (issue 699).
//!
//! One module per method-name family; each exposes one `handle_<family>` that
//! re-routes the family's verbs over the SAME `method::` constants. Pure code
//! motion — the dispatch ORDER (the one list in `handle_line`) and every
//! arm's bytes are unchanged.

pub(crate) mod account;
pub(crate) mod arb;
pub(crate) mod backtest;
pub(crate) mod blueprint;
pub(crate) mod engine;
pub(crate) mod execution_policy;
pub(crate) mod extension;
pub(crate) mod intent;
pub(crate) mod kline;
pub(crate) mod market;
pub(crate) mod misc;
pub(crate) mod order;
pub(crate) mod position;
pub(crate) mod risk;
pub(crate) mod round;
pub(crate) mod se;
pub(crate) mod strategy;
pub(crate) mod system;
pub(crate) mod trades;
pub(crate) mod unified;

pub(crate) mod prelude;
