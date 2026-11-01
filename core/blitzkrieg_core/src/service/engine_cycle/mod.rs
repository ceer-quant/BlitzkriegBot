//! The self-driving strategy engine's integration with the core, moved
//! verbatim from `service.rs` (pure code motion, no logic edits): the
//! market-data → engine feed, the Lua evaluation cycle with its replay
//! fast-path gates, the sizing/risk pre-filters (systemic facts, portfolio
//! and per-strategy limits, virtual-account quota, the cross-venue arb
//! pre-check), the diagnostics/stats readouts and the feed-health probes.
//!
//! #780: split into flat siblings by domain -- `lifecycle` (engine
//! wiring and feed-health probes), `ingest` (market-data feed),
//! `evaluate` (Lua evaluation cycle and replay fast path),
//! `risk_filters` (systemic/portfolio/strategy limits, virtual quota and
//! the arb pre-check) and `stats_views` (stats readouts and view
//! assembly). Pure code motion: the `impl Core` blocks stay inside
//! `service`, so the struct fields remain private to it.

use crate::engine::BatchBook;
use crate::model::*;
use crate::schema::Event;
use crate::shadow_evolution::EvolutionOutcome;
use crate::sim::{Book, rests_on_book};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

use super::Core;
use super::FAST_EVAL_FALLBACK_MS;
use super::config::StrategyLimit;
use super::helpers::{
    ARB_STRATEGY, SWEEP_FAILURE_FREEZE, arb_name_branch_warn_once, budget_to_shares,
    classify_rejection, strategy_acc_key,
};
use super::helpers::{
    CycleLeg, StrategyAccounting, dec_json, per_account_shares, virtual_batch_commit_impl,
};
use super::views::{AssetBooksView, BookLevelView, BookSideView, MarketPriceView, RoundView};
use crate::market::{venue_credentials_missing, venue_credentials_ready};

mod evaluate;
mod ingest;
mod lifecycle;
mod risk_filters;
mod stats_views;
