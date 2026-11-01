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
use crate::ipc::schema::Event;
use crate::model::*;
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

mod evaluate;
mod ingest;
mod lifecycle;
mod risk_filters;
mod stats_views;

/// RFC #541 方案 2a-i 挂钩 B（原型）的凭证谓词：`plugin` 的全部**声明**凭证
/// 变量（issue 648 的声明表，boot 从 `required_credential_env` 聚合进
/// config）是否都能从本进程环境解析——与
/// [`crate::market::credentials_status`] 逐变量同一谓词。声明表里没有该插件
/// （或表为空）⇒ 视为就绪（数据面无凭证的插件没有可失败的维度——
/// `credentials_status` 的 `ready = !required.is_empty()` 语义只属于面板行，
/// 这里不能把一个无凭证插件的 venue 判成永远未就绪）。
///
/// #824 cycle 2: moved verbatim from `market` into its only consumer (this
/// module's risk pre-filters) — a pure `std::env` predicate with no market
/// seam behind it.
pub fn venue_credentials_ready(
    plugin: &str,
    declared: &std::collections::BTreeMap<String, Vec<String>>,
) -> bool {
    venue_credentials_missing(plugin, declared).is_empty()
}

/// The refusal-diagnosis half of [`venue_credentials_ready`]: WHICH of the
/// plugin's declared vars do not resolve from the process environment, in the
/// plugin's declared order. RFC #541 边界三（拒绝可见性）：
/// an operator must be able to fix a wholesale batch refusal from the log line
/// alone — "kalshi 缺 KALSHI_RSA_PRIVATE_KEY"，不是笼统的"批校验失败"。
pub fn venue_credentials_missing(
    plugin: &str,
    declared: &std::collections::BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    declared
        .get(plugin)
        .map(|vars| {
            vars.iter()
                .filter(|v| !std::env::var_os(v).is_some_and(|s| !s.is_empty()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}
